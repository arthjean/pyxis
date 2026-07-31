//! Classifies a shell command line into what it does, for display only.
//!
//! An agent's turn is mostly reading: `cat`, `sed -n`, `rg`, `ls`. Printing the
//! raw command line for each of those buries the one command that actually
//! changed something. Classifying them lets the transcript collapse a run of
//! reads into a single `Explored` block and keep full command lines for the rest.
//!
//! This is a display heuristic, never a security boundary: a command that fails
//! to parse falls back to [`ParsedCommand::Unknown`], which renders verbatim.
//! Sandboxing and approvals work on the real command, not on this.
//!
//! Structurally derived from `codex-rs/shell-command/src/parse_command.rs`
//! (Apache-2.0); the classification rules are the same, the implementation is
//! written against Pyxis types and covers the commands agents actually emit.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedCommand {
    /// Reads a file whole or in part.
    Read { cmd: String, name: String },
    /// Lists a directory.
    ListFiles { cmd: String, path: Option<String> },
    /// Searches file contents or names.
    Search {
        cmd: String,
        query: Option<String>,
        path: Option<String>,
    },
    /// Anything else, shown as typed.
    Unknown { cmd: String },
}

impl ParsedCommand {
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::Read { .. } | Self::ListFiles { .. } | Self::Search { .. }
        )
    }
}

/// Commands that only reshape the output of the previous pipeline stage. Their
/// presence must not turn a classified pipeline into an opaque one.
const PIPELINE_FILTERS: &[&str] = &[
    "head", "tail", "wc", "sort", "uniq", "cut", "tr", "awk", "sed", "column", "nl", "xargs",
    "jq", "grep", "rg",
];

const READ_COMMANDS: &[&str] = &["cat", "bat", "batcat", "less", "more", "head", "tail", "view"];
const LIST_COMMANDS: &[&str] = &["ls", "eza", "exa", "tree", "du"];
const SEARCH_COMMANDS: &[&str] = &["rg", "grep", "ag", "ack", "fd", "fdfind"];

/// Classifies `command`, one entry per meaningful pipeline stage.
///
/// Returns a single [`ParsedCommand::Unknown`] holding the whole line whenever a
/// stage cannot be classified: a pipeline is only as readable as its least
/// readable stage, and showing "Read x" for something that also writes would
/// misrepresent what ran.
pub fn parse_command(command: &str) -> Vec<ParsedCommand> {
    let command = command.trim();
    if command.is_empty() {
        return Vec::new();
    }
    let script = unwrap_shell_wrapper(command).unwrap_or_else(|| command.to_string());
    let unknown = || {
        vec![ParsedCommand::Unknown {
            cmd: command.to_string(),
        }]
    };

    // A redirection or a command substitution can write anywhere and run
    // anything; neither is visible in the stage that appears to be a plain read.
    if has_unquoted_effect(&script) {
        return unknown();
    }

    let stages = split_stages(&script);
    if stages.is_empty() {
        return unknown();
    }

    let mut parsed = Vec::new();
    for stage in &stages {
        let tokens = tokenize(stage);
        let tokens = strip_leading_cd(&tokens);
        if tokens.is_empty() {
            continue;
        }
        match classify(&tokens, stage) {
            Some(entry) => parsed.push(entry),
            // A filter after a classified stage adds nothing to the summary;
            // anywhere else it means the line does something we cannot name.
            None if !parsed.is_empty() && is_pipeline_filter(&tokens) => {}
            None => return unknown(),
        }
    }

    if parsed.is_empty() { unknown() } else { parsed }
}

/// Unwraps `bash -lc "…"` and friends, returning the inner script.
///
/// The wrapper is how a model asks for a shell, not something the reader needs
/// to see: what ran is the script inside it.
fn unwrap_shell_wrapper(command: &str) -> Option<String> {
    let tokens = tokenize(command);
    let (shell, rest) = tokens.split_first()?;
    let shell = Path::new(shell).file_name()?.to_str()?;
    if !matches!(shell, "bash" | "sh" | "zsh" | "dash") {
        return None;
    }
    // The script follows the flag bundle; `-c`, `-lc` and `-lic` all end in `c`.
    let script_index = rest
        .iter()
        .position(|arg| arg.starts_with('-') && arg.len() > 1 && arg.ends_with('c'))?
        + 1;
    rest.get(script_index).cloned()
}

/// Whether the script redirects, substitutes a command, or expands a backtick
/// outside quotes.
fn has_unquoted_effect(script: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut chars = script.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            // Command substitution still expands inside double quotes.
            Some('"') if ch == '$' && chars.peek() == Some(&'(') => return true,
            Some('"') if ch == '`' => return true,
            Some(open) => {
                if ch == open {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => {
                    chars.next();
                }
                '>' | '<' | '`' => return true,
                '$' if chars.peek() == Some(&'(') => return true,
                _ => {}
            },
        }
    }
    false
}

/// Splits on `|`, `&&`, `||` and `;`, ignoring separators inside quotes.
fn split_stages(script: &str) -> Vec<String> {
    let mut stages = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = script.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some(open) => {
                current.push(ch);
                if ch == open {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    current.push(ch);
                }
                '\\' => {
                    current.push(ch);
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                '|' | '&' => {
                    if chars.peek() == Some(&ch) {
                        chars.next();
                    }
                    stages.push(std::mem::take(&mut current));
                }
                ';' | '\n' => stages.push(std::mem::take(&mut current)),
                _ => current.push(ch),
            },
        }
    }
    stages.push(current);
    stages
        .into_iter()
        .map(|stage| stage.trim().to_string())
        .filter(|stage| !stage.is_empty())
        .collect()
}

/// Minimal shell-style tokenizer: splits on whitespace, honours quotes and
/// backslash escapes, and drops the quotes from the result.
fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => current.push(ch),
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    has_token = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                        has_token = true;
                    }
                }
                ch if ch.is_whitespace() => {
                    if has_token || !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                        has_token = false;
                    }
                }
                _ => {
                    current.push(ch);
                    has_token = true;
                }
            },
        }
    }
    if has_token || !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Drops a leading `cd <dir> &&`-style prefix, already split into its own stage
/// by `split_stages`, or an inline `cd dir` operand list.
fn strip_leading_cd(tokens: &[String]) -> Vec<String> {
    if tokens.first().map(String::as_str) == Some("cd") {
        return Vec::new();
    }
    tokens.to_vec()
}

fn is_pipeline_filter(tokens: &[String]) -> bool {
    tokens
        .first()
        .and_then(|token| base_name(token))
        .is_some_and(|name| PIPELINE_FILTERS.contains(&name))
}

fn base_name(token: &str) -> Option<&str> {
    Path::new(token).file_name()?.to_str()
}

fn classify(tokens: &[String], stage: &str) -> Option<ParsedCommand> {
    let name = base_name(tokens.first()?)?;
    let args = &tokens[1..];
    let cmd = stage.to_string();

    if name == "git" {
        return match args.first().map(String::as_str) {
            Some("grep") => Some(ParsedCommand::Search {
                cmd,
                query: first_operand(&args[1..]).map(str::to_string),
                path: None,
            }),
            Some("ls-files") => Some(ParsedCommand::ListFiles {
                cmd,
                path: first_operand(&args[1..]).map(str::to_string),
            }),
            _ => None,
        };
    }

    if SEARCH_COMMANDS.contains(&name) {
        let operands = operands(args);
        // `fd`/`rg --files` list paths rather than matching content.
        let lists_only = args.iter().any(|arg| arg == "--files") || matches!(name, "fd" | "fdfind");
        if lists_only {
            return Some(ParsedCommand::ListFiles {
                cmd,
                path: operands.first().map(|op| op.to_string()),
            });
        }
        return Some(ParsedCommand::Search {
            cmd,
            query: operands.first().map(|op| op.to_string()),
            path: operands.get(1).map(|op| op.to_string()),
        });
    }

    if name == "find" {
        let has_pattern = args.iter().any(|arg| matches!(arg.as_str(), "-name" | "-iname" | "-path"));
        let path = first_operand(args).map(str::to_string);
        return Some(if has_pattern {
            ParsedCommand::Search {
                cmd,
                query: args
                    .iter()
                    .skip_while(|arg| !matches!(arg.as_str(), "-name" | "-iname" | "-path"))
                    .nth(1)
                    .cloned(),
                path,
            }
        } else {
            ParsedCommand::ListFiles { cmd, path }
        });
    }

    if LIST_COMMANDS.contains(&name) {
        return Some(ParsedCommand::ListFiles {
            cmd,
            path: first_operand(args).map(str::to_string),
        });
    }

    if READ_COMMANDS.contains(&name) {
        // `head`/`tail` without an operand filter a pipe rather than read a file.
        let file = operands(args).into_iter().find(|op| looks_like_path(op))?;
        return Some(ParsedCommand::Read {
            cmd,
            name: display_name(file),
        });
    }

    None
}

/// Arguments that are neither flags nor flag values.
fn operands(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            continue;
        }
        if arg.starts_with('-') {
            // `-n 5`, `-C 3`: a short flag whose value is the next argument.
            skip_next = arg.len() == 2 && arg.as_bytes()[1].is_ascii_alphabetic() && !is_boolean_flag(arg);
            continue;
        }
        out.push(arg.as_str());
    }
    out
}

fn first_operand(args: &[String]) -> Option<&str> {
    operands(args).into_iter().next()
}

/// Short flags that never take a value, so the token after them is an operand.
fn is_boolean_flag(arg: &str) -> bool {
    matches!(arg, "-l" | "-a" | "-r" | "-R" | "-i" | "-v" | "-h" | "-p" | "-f" | "-s")
}

fn looks_like_path(operand: &str) -> bool {
    !operand.is_empty() && !operand.chars().all(|ch| ch.is_ascii_digit())
}

/// Keeps the last two path segments: enough to tell `src/lib.rs` from
/// `tests/lib.rs` without spending a line on an absolute path.
fn display_name(path: &str) -> String {
    let normalized = path.trim_end_matches('/');
    let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    match segments.len() {
        0 => normalized.to_string(),
        1 => segments[0].to_string(),
        _ => segments[segments.len() - 2..].join("/"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(command: &str) -> Vec<ParsedCommand> {
        parse_command(command)
    }

    #[test]
    fn cat_is_a_read() {
        assert_eq!(
            parse("cat src/lib.rs"),
            vec![ParsedCommand::Read {
                cmd: "cat src/lib.rs".into(),
                name: "src/lib.rs".into()
            }]
        );
    }

    #[test]
    fn a_shell_wrapper_is_unwrapped() {
        let parsed = parse("bash -lc \"ls crates\"");
        assert_eq!(
            parsed,
            vec![ParsedCommand::ListFiles {
                cmd: "ls crates".into(),
                path: Some("crates".into())
            }]
        );
    }

    #[test]
    fn ripgrep_carries_its_query_and_path() {
        assert_eq!(
            parse("rg --line-number ChatSurface crates/agent-tui"),
            vec![ParsedCommand::Search {
                cmd: "rg --line-number ChatSurface crates/agent-tui".into(),
                query: Some("ChatSurface".into()),
                path: Some("crates/agent-tui".into()),
            }]
        );
    }

    #[test]
    fn a_leading_cd_is_dropped() {
        assert_eq!(
            parse("cd crates/agent-tui && cat src/lib.rs"),
            vec![ParsedCommand::Read {
                cmd: "cat src/lib.rs".into(),
                name: "src/lib.rs".into()
            }]
        );
    }

    /// A filter after a classified stage describes the same read, so it must not
    /// drag the whole line into `Unknown`.
    #[test]
    fn a_trailing_filter_keeps_the_stage_classification() {
        assert_eq!(
            parse("cat src/lib.rs | head -n 40"),
            vec![ParsedCommand::Read {
                cmd: "cat src/lib.rs".into(),
                name: "src/lib.rs".into()
            }]
        );
    }

    /// Anything that can write must render verbatim: summarizing it as a read
    /// would tell the reader the opposite of what happened.
    #[test]
    fn an_unclassifiable_stage_falls_back_to_the_whole_line() {
        let command = "cat src/lib.rs > /tmp/copy";
        assert_eq!(
            parse(command),
            vec![ParsedCommand::Unknown {
                cmd: command.into()
            }]
        );
    }

    #[test]
    fn a_mutating_command_is_unknown() {
        assert_eq!(
            parse("cargo build --workspace"),
            vec![ParsedCommand::Unknown {
                cmd: "cargo build --workspace".into()
            }]
        );
    }

    #[test]
    fn quotes_hide_separators_from_the_splitter() {
        assert_eq!(
            parse("rg 'a && b' src"),
            vec![ParsedCommand::Search {
                cmd: "rg 'a && b' src".into(),
                query: Some("a && b".into()),
                path: Some("src".into()),
            }]
        );
    }

    #[test]
    fn head_with_a_count_still_names_the_file() {
        assert_eq!(
            parse("head -n 20 README.md"),
            vec![ParsedCommand::Read {
                cmd: "head -n 20 README.md".into(),
                name: "README.md".into()
            }]
        );
    }

    #[test]
    fn find_with_a_name_pattern_is_a_search() {
        assert_eq!(
            parse("find crates -name '*.rs'"),
            vec![ParsedCommand::Search {
                cmd: "find crates -name '*.rs'".into(),
                query: Some("*.rs".into()),
                path: Some("crates".into()),
            }]
        );
    }

    #[test]
    fn a_deep_path_keeps_its_last_two_segments() {
        assert_eq!(display_name("crates/agent-tui/src/lib.rs"), "src/lib.rs");
        assert_eq!(display_name("lib.rs"), "lib.rs");
    }

    #[test]
    fn an_empty_command_parses_to_nothing() {
        assert!(parse("   ").is_empty());
    }

    /// A command substitution runs code the classification never sees.
    #[test]
    fn a_command_substitution_is_unknown() {
        let command = "cat $(find . -name secret)";
        assert_eq!(
            parse(command),
            vec![ParsedCommand::Unknown {
                cmd: command.into()
            }]
        );
    }

    #[test]
    fn a_redirection_inside_quotes_is_not_an_effect() {
        assert_eq!(
            parse("rg '>' src"),
            vec![ParsedCommand::Search {
                cmd: "rg '>' src".into(),
                query: Some(">".into()),
                path: Some("src".into()),
            }]
        );
    }

    #[test]
    fn successive_reads_each_get_an_entry() {
        assert_eq!(
            parse("cat a.rs && cat b.rs"),
            vec![
                ParsedCommand::Read {
                    cmd: "cat a.rs".into(),
                    name: "a.rs".into()
                },
                ParsedCommand::Read {
                    cmd: "cat b.rs".into(),
                    name: "b.rs".into()
                },
            ]
        );
    }
}
