//! Static classification of a raw shell command (US-007). Answers two
//! questions the approval pipeline asks before running `bash`:
//!
//! 1. can this command be proven side-effect free, so that no confirmation is
//!    needed (`CommandClass::SideEffectFree`)?
//! 2. can an answer given by the user be remembered for the session, and under
//!    which key (`CommandClass::Argv`, US-008)?
//!
//! The analysis is deliberately NOT a shell parser. It recognizes the
//! constructs that make any static reasoning unsound (composition, redirection,
//! substitution, expansion, quoting) and **disqualifies** the command as soon as
//! one appears: the decision we are looking for is a refusal by default, so a
//! full parser would buy a much larger error surface than the need.
//!
//! Security: matching always happens on TOKEN sequences, never on a substring of
//! the raw command. Substring matching is the exact vector of CVE-2026-22708,
//! where an approved `git` let `git push --force` through.

/// What a static analysis of a raw command can prove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandClass {
    /// Plain argv whose program AND arguments belong to the side-effect-free
    /// set: runs without confirmation.
    SideEffectFree(Vec<String>),
    /// Plain argv, nothing proven: confirmation required. The token sequence is
    /// exact, hence usable as a session approval key.
    Argv(Vec<String>),
    /// The command carries a shell construct: confirmation required, and the
    /// answer is never remembered. Carries the user-facing reason.
    Opaque(&'static str),
}

impl CommandClass {
    /// Token sequence when the command is plain argv.
    pub fn tokens(&self) -> Option<&[String]> {
        match self {
            Self::SideEffectFree(tokens) | Self::Argv(tokens) => Some(tokens),
            Self::Opaque(_) => None,
        }
    }
}

// Reasons shown to the user when a command cannot be remembered. Phrased as
// clauses so the frontend can prefix them ("not rememberable: ...").
const CHAINED: &str = "the command chains or pipes several commands";
const REDIRECTED: &str = "the command redirects a stream";
const SUBSTITUTION: &str = "the command contains a substitution or a variable";
const SUBSHELL: &str = "the command opens a subshell";
const EXPANSION: &str = "the command contains a glob or an expansion";
const QUOTED: &str = "the command contains quotes";
const ESCAPED: &str = "the command contains an escape";
const HISTORY: &str = "the command contains a history expansion";
const COMMENT: &str = "the command contains a comment";
const CONTROL: &str = "the command contains a control character";
const EMPTY: &str = "the command is empty";

/// One program a user declared side-effect free, with the same two constraints
/// the built-in table uses. Owned, because it comes from a configuration file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SafeCommand {
    /// Matched EXACTLY against the first token, like a built-in entry: a path
    /// (`/usr/local/bin/just`) never matches, because it could designate
    /// anything.
    pub program: String,
    /// When non-empty, the second token must be one of these, and no global flag
    /// may precede it.
    pub subcommands: Vec<String>,
    /// Flags that disqualify the call wherever they appear.
    pub denied: Vec<String>,
}

/// User-declared additions to the side-effect-free set (`safe_commands`).
///
/// A project has tools the built-in table cannot know about (`just --list`,
/// `mise ls`, an internal script), and today each of them costs a confirmation
/// on every call. This is the smallest thing that fixes it: a list of programs,
/// with the same two constraints an entry of the built-in table carries.
///
/// It is deliberately NOT a policy language. Codex has one (`codex-execpolicy`)
/// with rules, decisions and amendments; the requirement here is "let me add
/// `just --list`", and a language would be a second security surface to get
/// right for a need a list already covers.
///
/// It can only WIDEN, and only from a file the workspace does not control
/// (`safe_commands` is a security key). Two rules make that safe:
/// - the built-in table wins on a program both define, so no configuration can
///   remove a `denied` flag the built-in entry carries;
/// - nothing here touches the tokenizer. A command with a shell construct stays
///   opaque whatever the configuration says, because the reasoning that makes
///   the classification sound is the absence of a shell, not the program name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandPolicy {
    extra: Vec<SafeCommand>,
}

impl CommandPolicy {
    /// Empty policy: the built-in table alone. What every caller gets by default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds from user entries. An entry whose program is empty, carries a path
    /// separator, or duplicates a built-in one is dropped, with the reason
    /// returned for the caller to report: silently ignoring a line the user
    /// wrote is how a configuration becomes untrustworthy.
    pub fn from_entries(entries: Vec<SafeCommand>) -> (Self, Vec<String>) {
        let mut extra = Vec::new();
        let mut rejected = Vec::new();
        for entry in entries {
            let program = entry.program.trim().to_string();
            if program.is_empty() {
                rejected.push("safe_commands: an entry has no program".to_string());
                continue;
            }
            if program.contains('/') || program.contains('\\') {
                rejected.push(format!(
                    "safe_commands: `{program}` looks like a path; declare the program name alone"
                ));
                continue;
            }
            if SIDE_EFFECT_FREE.iter().any(|safe| safe.program == program) {
                // Refused rather than merged: a merge would let a configuration
                // widen a built-in entry by omitting one of its denied flags.
                rejected.push(format!(
                    "safe_commands: `{program}` is already known; its built-in rule is kept"
                ));
                continue;
            }
            if extra
                .iter()
                .any(|other: &SafeCommand| other.program == program)
            {
                rejected.push(format!("safe_commands: `{program}` declared twice"));
                continue;
            }
            extra.push(SafeCommand {
                program,
                subcommands: entry.subcommands,
                denied: entry.denied,
            });
        }
        (Self { extra }, rejected)
    }

    /// Programs the user added, for `/status` and diagnostics.
    pub fn programs(&self) -> Vec<&str> {
        self.extra
            .iter()
            .map(|safe| safe.program.as_str())
            .collect()
    }

    fn find(&self, program: &str) -> Option<&SafeCommand> {
        self.extra.iter().find(|safe| safe.program == program)
    }
}

/// Classifies a raw command against the built-in table alone.
pub fn classify(command: &str) -> CommandClass {
    classify_with(command, &CommandPolicy::new())
}

/// Classifies a raw command. Pure and allocation-bounded: called on the
/// permission path of every `bash` call.
pub fn classify_with(command: &str, policy: &CommandPolicy) -> CommandClass {
    let tokens = match tokenize(command) {
        Ok(tokens) => tokens,
        Err(reason) => return CommandClass::Opaque(reason),
    };
    if tokens.is_empty() {
        return CommandClass::Opaque(EMPTY);
    }
    if is_side_effect_free(&tokens, policy) {
        CommandClass::SideEffectFree(tokens)
    } else {
        CommandClass::Argv(tokens)
    }
}

/// Splits a command into argv tokens, or names the shell construct that makes
/// the command opaque. Whitespace is the ONLY separator: any character that a
/// shell would interpret disqualifies the command outright.
fn tokenize(command: &str) -> Result<Vec<String>, &'static str> {
    for c in command.chars() {
        if let Some(reason) = disqualifier(c) {
            return Err(reason);
        }
    }
    Ok(command.split_whitespace().map(str::to_string).collect())
}

/// Characters a shell interprets. Anything unlisted is a literal token
/// character (letters, digits, `-`, `_`, `.`, `/`, `=`, `,`, `+`, `:`, `@`,
/// and non-ASCII text).
fn disqualifier(c: char) -> Option<&'static str> {
    match c {
        // Newlines separate commands: checked before the whitespace shortcut.
        '\n' | '\r' => Some(CHAINED),
        _ if c.is_whitespace() => None,
        ';' | '&' | '|' => Some(CHAINED),
        '<' | '>' => Some(REDIRECTED),
        '$' | '`' => Some(SUBSTITUTION),
        '(' | ')' => Some(SUBSHELL),
        '{' | '}' | '*' | '?' | '[' | ']' | '~' => Some(EXPANSION),
        '\'' | '"' => Some(QUOTED),
        '\\' => Some(ESCAPED),
        '!' => Some(HISTORY),
        '#' => Some(COMMENT),
        _ if c.is_control() => Some(CONTROL),
        _ => None,
    }
}

/// A program of the side-effect-free set, with the constraints that keep it so.
struct SafeProgram {
    /// Matched EXACTLY against the first token. A path (`/bin/ls`, `./ls`) never
    /// matches: it could designate anything.
    program: &'static str,
    /// When non-empty, the second token must be one of these read-only
    /// subcommands, and no global flag may precede it.
    subcommands: &'static [&'static str],
    /// Flags that disqualify the call wherever they appear, because they make
    /// the program write or execute something.
    denied: &'static [&'static str],
}

const fn plain(program: &'static str) -> SafeProgram {
    SafeProgram {
        program,
        subcommands: &[],
        denied: &[],
    }
}

/// Programs whose every documented invocation only reads. Deliberately short:
/// each entry is an explicit decision, and a missing program only costs one
/// confirmation. Notable exclusions: `sort`/`uniq`/`tee` (write through an
/// operand or `-o`), `sed`/`awk` (in-place edit, `w` command, `system()`),
/// `env`/`xargs` (run another program), `cargo` (refreshes `Cargo.lock` and may
/// hit the network).
const SIDE_EFFECT_FREE: &[SafeProgram] = &[
    plain("ls"),
    plain("pwd"),
    plain("whoami"),
    plain("id"),
    plain("uname"),
    plain("df"),
    plain("du"),
    plain("wc"),
    plain("file"),
    plain("stat"),
    plain("head"),
    plain("tail"),
    plain("cat"),
    plain("nl"),
    plain("basename"),
    plain("dirname"),
    plain("realpath"),
    plain("readlink"),
    SafeProgram {
        program: "tree",
        subcommands: &[],
        // `-o file` writes the listing to disk.
        denied: &["-o", "--output"],
    },
    SafeProgram {
        program: "date",
        subcommands: &[],
        // `-s` sets the system clock.
        denied: &["-s", "--set"],
    },
    plain("which"),
    plain("echo"),
    plain("printf"),
    plain("cut"),
    plain("column"),
    plain("diff"),
    plain("cmp"),
    plain("grep"),
    plain("egrep"),
    plain("fgrep"),
    plain("ps"),
    plain("free"),
    plain("uptime"),
    plain("md5sum"),
    plain("sha1sum"),
    plain("sha256sum"),
    SafeProgram {
        program: "find",
        subcommands: &[],
        denied: &[
            "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf",
            "-fls",
        ],
    },
    SafeProgram {
        program: "rg",
        subcommands: &[],
        // `--pre` runs a preprocessor program, `--hostname-bin` runs a binary.
        denied: &["--pre", "--hostname-bin"],
    },
    SafeProgram {
        program: "fd",
        subcommands: &[],
        denied: &["-x", "-X", "--exec", "--exec-batch"],
    },
    SafeProgram {
        program: "git",
        subcommands: &[
            "status",
            "log",
            "diff",
            "show",
            "blame",
            "shortlog",
            "describe",
            "rev-parse",
            "ls-files",
            "ls-tree",
            "cat-file",
            "whatchanged",
        ],
        // `-c`/`--config-env` inject a configuration that turns a read into an
        // execution (`core.pager`, `diff.external`); `--output` writes a file;
        // `--upload-pack`/`--receive-pack`/`--exec-path` run a program.
        denied: &[
            "-c",
            "--config-env",
            "--exec-path",
            "--output",
            "--ext-diff",
            "--upload-pack",
            "--receive-pack",
            "-O",
        ],
    },
];

/// True when the program AND every argument belong to the side-effect-free set.
///
/// The built-in table is consulted FIRST: a program it defines keeps its
/// built-in rule whatever a configuration says.
fn is_side_effect_free(tokens: &[String], policy: &CommandPolicy) -> bool {
    let Some(program) = tokens.first() else {
        return false;
    };
    let (subcommands, denied): (Vec<&str>, Vec<&str>) =
        match SIDE_EFFECT_FREE.iter().find(|p| p.program == program) {
            Some(safe) => (safe.subcommands.to_vec(), safe.denied.to_vec()),
            None => match policy.find(program) {
                Some(safe) => (
                    safe.subcommands.iter().map(String::as_str).collect(),
                    safe.denied.iter().map(String::as_str).collect(),
                ),
                None => return false,
            },
        };
    let mut args = &tokens[1..];
    if !subcommands.is_empty() {
        // The subcommand must come FIRST: a global flag before it (`git -C dir
        // status`) is refused rather than skipped, because skipping it would
        // require knowing which flags consume a value.
        let Some(sub) = args.first() else {
            return false;
        };
        if !subcommands.contains(&sub.as_str()) {
            return false;
        }
        args = &args[1..];
    }
    !args.iter().any(|arg| is_denied_flag(arg, &denied))
}

/// Does this token carry one of the denied flags? Covers the three spellings a
/// flag can take: exact (`-x`), valued (`--output=f`), and a short flag inside a
/// cluster (`-Hx`).
fn is_denied_flag(token: &str, denied: &[&str]) -> bool {
    denied.iter().any(|flag| {
        if token == *flag {
            return true;
        }
        if let Some(rest) = token.strip_prefix(flag)
            && rest.starts_with('=')
        {
            return true;
        }
        let is_short = flag.len() == 2 && flag.starts_with('-');
        let clustered = token.starts_with('-') && !token.starts_with("--");
        if is_short
            && clustered
            && let Some(letter) = flag.chars().nth(1)
        {
            return token[1..].contains(letter);
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(command: &str) -> Vec<String> {
        classify(command)
            .tokens()
            .map(<[String]>::to_vec)
            .unwrap_or_default()
    }

    #[test]
    fn plain_reads_are_side_effect_free() {
        for command in [
            "ls",
            "ls -la",
            "pwd",
            "git status",
            "git log --oneline -20",
            "git diff HEAD",
            "cat Cargo.toml",
            "rg permission crates/",
            "find . -name Cargo.toml",
            "wc -l src/lib.rs",
        ] {
            assert!(
                matches!(classify(command), CommandClass::SideEffectFree(_)),
                "{command} should run without confirmation"
            );
        }
    }

    #[test]
    fn mutating_and_unknown_programs_still_ask() {
        for command in [
            "rm -rf target",
            "git push --force",
            "git commit -m fix",
            "cargo test",
            "curl https://example.com",
            "./script.sh",
            "/bin/ls",
        ] {
            assert!(
                matches!(classify(command), CommandClass::Argv(_)),
                "{command} should require a confirmation"
            );
        }
    }

    #[test]
    fn shell_constructs_are_never_side_effect_free() {
        // US-007 AC3: a read program does not launder a composed command.
        for (command, reason) in [
            ("ls && rm -rf build", CHAINED),
            ("ls; rm -rf build", CHAINED),
            ("ls | head", CHAINED),
            ("ls &", CHAINED),
            ("ls\nrm -rf build", CHAINED),
            ("cat file > out", REDIRECTED),
            ("cat < in", REDIRECTED),
            ("ls $HOME", SUBSTITUTION),
            ("ls $(pwd)", SUBSTITUTION),
            ("ls `pwd`", SUBSTITUTION),
            ("(ls)", SUBSHELL),
            ("ls *.rs", EXPANSION),
            ("ls ~", EXPANSION),
            ("ls {a,b}", EXPANSION),
            ("git log --format='%h'", QUOTED),
            ("ls \"a b\"", QUOTED),
            ("ls a\\ b", ESCAPED),
            ("ls !42", HISTORY),
            ("ls # comment", COMMENT),
            ("ls \u{7}", CONTROL),
        ] {
            assert_eq!(
                classify(command),
                CommandClass::Opaque(reason),
                "{command} must stay opaque"
            );
        }
    }

    #[test]
    fn dangerous_flags_disqualify_a_safe_program() {
        for command in [
            "find . -delete",
            "find . -exec rm -rf {} +",
            "find . -execdir rm -rf .",
            "fd -x rm",
            "fd --exec-batch rm",
            "rg --pre=cat pattern",
            "rg --hostname-bin id",
            "git -c core.pager=id log",
            "git log --output=/tmp/x",
            "tree -o listing.txt",
            "date -s 2020-01-01",
        ] {
            assert!(
                !matches!(classify(command), CommandClass::SideEffectFree(_)),
                "{command} must not be classified side-effect free"
            );
        }
    }

    #[test]
    fn git_write_subcommands_are_not_in_the_set() {
        for command in [
            "git push",
            "git commit",
            "git checkout main",
            "git branch -D topic",
            "git config user.email a@b.c",
            "git -C /tmp status",
        ] {
            assert!(
                !matches!(classify(command), CommandClass::SideEffectFree(_)),
                "{command} must not be classified side-effect free"
            );
        }
    }

    #[test]
    fn tokens_are_exact_and_never_substrings() {
        // US-008 AC2: a shared string prefix is NOT a shared token sequence.
        assert_eq!(tokens("git status"), vec!["git", "status"]);
        assert_ne!(tokens("git status"), tokens("git status-x"));
        assert_eq!(tokens("ls   -la    src"), vec!["ls", "-la", "src"]);
    }

    #[test]
    fn empty_command_is_opaque() {
        assert_eq!(classify("   "), CommandClass::Opaque(EMPTY));
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    fn policy(entries: Vec<SafeCommand>) -> CommandPolicy {
        CommandPolicy::from_entries(entries).0
    }

    #[test]
    fn a_declared_program_stops_asking_for_confirmation() {
        let just = SafeCommand {
            program: "just".to_string(),
            subcommands: vec!["--list".to_string()],
            denied: Vec::new(),
        };
        let policy = policy(vec![just]);
        assert!(matches!(
            classify_with("just --list", &policy),
            CommandClass::SideEffectFree(_)
        ));
        // The subcommand constraint applies to a declared entry exactly as it
        // does to a built-in one.
        assert!(matches!(
            classify_with("just deploy", &policy),
            CommandClass::Argv(_)
        ));
        // And the built-in table alone still asks.
        assert!(matches!(classify("just --list"), CommandClass::Argv(_)));
    }

    #[test]
    fn a_configuration_cannot_weaken_a_built_in_entry() {
        // The attack this refuses: redeclaring `git` without its denied flags
        // would make `git -c core.pager=sh log` run without a question.
        let (policy, rejected) = CommandPolicy::from_entries(vec![SafeCommand {
            program: "git".to_string(),
            subcommands: vec!["log".to_string()],
            denied: Vec::new(),
        }]);
        assert!(policy.programs().is_empty());
        assert!(rejected[0].contains("already known"), "{rejected:?}");
        assert!(
            matches!(
                classify_with("git -c core.pager=sh log", &policy),
                CommandClass::Argv(_)
            ),
            "the built-in rule must keep winning"
        );
    }

    #[test]
    fn a_configuration_never_touches_the_tokenizer() {
        // Whatever a user declares, a command carrying a shell construct stays
        // opaque: what makes the classification sound is the absence of a shell,
        // not the program name.
        let policy = policy(vec![SafeCommand {
            program: "just".to_string(),
            subcommands: Vec::new(),
            denied: Vec::new(),
        }]);
        for command in ["just --list | sh", "just $(id)", "just --list > /tmp/x"] {
            assert!(
                matches!(classify_with(command, &policy), CommandClass::Opaque(_)),
                "{command} must stay opaque"
            );
        }
    }

    #[test]
    fn malformed_entries_are_reported_rather_than_dropped_silently() {
        let (policy, rejected) = CommandPolicy::from_entries(vec![
            SafeCommand {
                program: "  ".to_string(),
                ..SafeCommand::default()
            },
            SafeCommand {
                program: "/usr/local/bin/just".to_string(),
                ..SafeCommand::default()
            },
            SafeCommand {
                program: "mise".to_string(),
                ..SafeCommand::default()
            },
            SafeCommand {
                program: "mise".to_string(),
                ..SafeCommand::default()
            },
        ]);
        assert_eq!(policy.programs(), vec!["mise"]);
        assert_eq!(rejected.len(), 3, "{rejected:?}");
        assert!(rejected.iter().any(|r| r.contains("looks like a path")));
        assert!(rejected.iter().any(|r| r.contains("declared twice")));
    }

    #[test]
    fn a_declared_denied_flag_still_disqualifies() {
        let policy = policy(vec![SafeCommand {
            program: "mytool".to_string(),
            subcommands: Vec::new(),
            denied: vec!["--write".to_string()],
        }]);
        assert!(matches!(
            classify_with("mytool --read", &policy),
            CommandClass::SideEffectFree(_)
        ));
        assert!(matches!(
            classify_with("mytool --write=out", &policy),
            CommandClass::Argv(_)
        ));
    }
}
