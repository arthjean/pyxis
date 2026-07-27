//! `apply_patch` tool (US-010): edits in the format the `*-codex` models were
//! trained on. Grammar taken from Codex
//! (`codex-rs/apply-patch/src/parser.rs`, official `apply_patch.lark`):
//!
//! ```text
//! *** Begin Patch
//! *** Add File: path        followed by "+" lines
//! *** Delete File: path
//! *** Update File: path     optional "*** Move to: path", then chunks
//!   @@ [context]            optional anchor
//!   " " context / "-" removed / "+" added
//!   *** End of File         the chunk ends the file
//! *** End Patch
//! ```
//!
//! **All-or-nothing (AC2).** Parsing, path guardrails and text application all
//! happen in memory; the first byte hits the disk only once every hunk has been
//! resolved. A stale context therefore leaves the workspace untouched, which is
//! the property that makes a failed patch safe to retry.
//!
//! Only the format is Codex's. The implementation is written independently (see
//! `docs/codex-port-inventory.md`) and plugs into the Pyxis guardrails:
//! `confine`, `guard_write_target`, `ensure_policy_allows_write`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ValidationError};
use crate::path::{
    confine, ensure_policy_allows_write, guard_write_target, remove_file_confined,
    replace_file_confined,
};
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{MAX_EDIT_FILE_BYTES, MAX_WRITE_BYTES, Tool, ToolCtx, ToolOutput};

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";

/// Bound on the patch text itself. A patch larger than a full file rewrite is
/// not a patch.
pub const MAX_PATCH_BYTES: usize = 1_000_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchInput {
    /// Full patch text, markers included. Named `input` like the Codex payload,
    /// so a model trained on that tool emits the same thing.
    pub input: String,
}

pub struct ApplyPatch;

#[async_trait]
impl Tool for ApplyPatch {
    type Input = ApplyPatchInput;

    fn name(&self) -> &str {
        "apply_patch"
    }
    fn description(&self) -> String {
        "Apply a patch to workspace files, in the apply_patch format. The text \
         opens with \"*** Begin Patch\" and closes with \"*** End Patch\"; \
         between them come \"*** Add File: <path>\" (lines prefixed with +), \
         \"*** Delete File: <path>\", or \"*** Update File: <path>\" followed by \
         chunks whose lines are prefixed with \" \" (context), \"-\" (removed) \
         or \"+\" (added), optionally anchored by a \"@@ <context>\" line. \
         Nothing is written unless every hunk applies. Parameter: input."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "Full patch text, from *** Begin Patch to *** End Patch."
                }
            },
            "required": ["input"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    fn returns_untrusted(&self) -> bool {
        false
    }
    fn behavioral_guidelines(&self) -> &[&'static str] {
        PATCH_GUIDELINES
    }
    /// The patch is parsed and every target checked BEFORE the permission
    /// decision (AC3): a protected subpath is refused ahead of any mode, exactly
    /// like `write` and `edit`.
    fn validate_input(&self, input: &Self::Input, ctx: &ToolCtx) -> Result<(), ValidationError> {
        if input.input.len() > MAX_PATCH_BYTES {
            return Err(ValidationError::new(format!(
                "patch too large: {} bytes > {MAX_PATCH_BYTES}",
                input.input.len()
            )));
        }
        let patch = parse_patch(&input.input).map_err(|e| ValidationError::new(e.to_string()))?;
        for path in patch.touched_paths() {
            guard_write_target(&ctx.sandbox, &ctx.workspace, &path)?;
        }
        Ok(())
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Ask
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let patch = parse_patch(&input.input).map_err(|e| ToolError::Rejected(e.to_string()))?;
        // Phase 1: resolve everything in memory. A failure here has touched no file.
        let mut writes: Vec<(PathBuf, String, String)> = Vec::new();
        let mut removals: Vec<(PathBuf, String)> = Vec::new();
        let mut summary: Vec<String> = Vec::new();
        for hunk in &patch.hunks {
            match hunk {
                Hunk::Add { path, contents } => {
                    let abs = resolve_target(ctx, path).await?;
                    if tokio::fs::metadata(&abs).await.is_ok() {
                        return Err(ToolError::Rejected(format!(
                            "{path} already exists: use *** Update File instead of *** Add File"
                        )));
                    }
                    if contents.len() > MAX_WRITE_BYTES {
                        return Err(ToolError::Rejected(format!(
                            "{path}: {} bytes > {MAX_WRITE_BYTES}",
                            contents.len()
                        )));
                    }
                    writes.push((abs, path.clone(), contents.clone()));
                    summary.push(format!("A {path}"));
                }
                Hunk::Delete { path } => {
                    let abs = resolve_target(ctx, path).await?;
                    if tokio::fs::metadata(&abs).await.is_err() {
                        return Err(ToolError::Rejected(format!(
                            "{path} does not exist: nothing to delete"
                        )));
                    }
                    removals.push((abs, path.clone()));
                    summary.push(format!("D {path}"));
                }
                Hunk::Update {
                    path,
                    move_to,
                    chunks,
                } => {
                    let abs = resolve_target(ctx, path).await?;
                    let meta = tokio::fs::metadata(&abs).await.map_err(|e| {
                        ToolError::Rejected(format!("{path}: {e} (nothing to update)"))
                    })?;
                    if meta.len() > MAX_EDIT_FILE_BYTES {
                        return Err(ToolError::Rejected(format!(
                            "{path} is too large to patch: {} bytes > {MAX_EDIT_FILE_BYTES}",
                            meta.len()
                        )));
                    }
                    let original = tokio::fs::read_to_string(&abs)
                        .await
                        .map_err(|e| ToolError::Io(format!("{path}: {e}")))?;
                    let updated = apply_chunks(&original, chunks)
                        .map_err(|e| ToolError::Rejected(format!("{path}: {e}")))?;
                    match move_to {
                        Some(dest) => {
                            let dest_abs = resolve_target(ctx, dest).await?;
                            writes.push((dest_abs, dest.clone(), updated));
                            removals.push((abs, path.clone()));
                            summary.push(format!("M {path} -> {dest}"));
                        }
                        None => {
                            writes.push((abs, path.clone(), updated));
                            summary.push(format!("M {path}"));
                        }
                    }
                }
            }
        }

        // A path written twice, or written AND deleted, has no defined outcome:
        // the hunks were each computed against the file as it is on disk, so the
        // second one would silently undo the first. Refused rather than
        // arbitrated, because "the last one wins" would be a data loss the model
        // never asked for.
        if let Some(clash) = first_clash(&writes, &removals) {
            return Err(ToolError::Rejected(format!(
                "{clash} is touched twice by this patch: put every change to a \
                 file in ONE hunk"
            )));
        }

        // Phase 2: the disk. Everything above is resolved, so a partial file
        // state can no longer come from a stale context (AC2).
        for (abs, display, contents) in &writes {
            replace_file_confined(&ctx.workspace, abs, display, contents.as_bytes()).await?;
        }
        for (abs, display) in &removals {
            remove_file_confined(&ctx.workspace, abs, display).await?;
        }
        Ok(ToolOutput::text(format!(
            "Patch applied ({} file(s)):\n{}",
            summary.len(),
            summary.join("\n")
        )))
    }
}

const PATCH_GUIDELINES: &[&str] = &[
    "apply_patch: prefer it over edit when you change several places in one \
     file or several files at once; edit stays the right tool for a single \
     unique anchor. A patch is all-or-nothing: if the context no longer \
     matches, NOTHING is written and you re-read the file before retrying.",
];

/// First absolute path that two operations of the same patch would fight over:
/// written twice, or written and deleted. Compared on the RESOLVED path, so a
/// move onto itself and two spellings of the same file are both caught.
fn first_clash(
    writes: &[(PathBuf, String, String)],
    removals: &[(PathBuf, String)],
) -> Option<String> {
    let mut seen: BTreeSet<&std::path::Path> = BTreeSet::new();
    for (abs, display, _) in writes {
        if !seen.insert(abs.as_path()) {
            return Some(display.clone());
        }
    }
    for (abs, display) in removals {
        if !seen.insert(abs.as_path()) {
            return Some(display.clone());
        }
    }
    None
}

/// Resolves a patch path against the workspace and re-checks the write policy
/// right before use, like `write` does: between validation and here, a symlink
/// may have appeared.
async fn resolve_target(ctx: &ToolCtx, path: &str) -> Result<PathBuf, ToolError> {
    let abs = confine(&ctx.workspace, path)?;
    ensure_policy_allows_write(&ctx.sandbox, &ctx.workspace, &abs, path)?;
    Ok(abs)
}

// ───────────────────────── parsing ─────────────────────────

#[derive(Debug, PartialEq, Eq)]
pub struct Patch {
    pub hunks: Vec<Hunk>,
}

impl Patch {
    /// Every path the patch writes to, destinations of a move included. Sorted
    /// and deduplicated: the guardrails are evaluated once per path.
    fn touched_paths(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for hunk in &self.hunks {
            match hunk {
                Hunk::Add { path, .. } | Hunk::Delete { path } => {
                    out.insert(path.clone());
                }
                Hunk::Update { path, move_to, .. } => {
                    out.insert(path.clone());
                    if let Some(dest) = move_to {
                        out.insert(dest.clone());
                    }
                }
            }
        }
        out
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<Chunk>,
    },
}

/// One contiguous change inside an `*** Update File` hunk.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct Chunk {
    /// Text of the `@@` line, used to position the search. `None` = no anchor.
    pub context: Option<String>,
    pub lines: Vec<PatchLine>,
    /// The chunk claims to end the file (`*** End of File`).
    pub end_of_file: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PatchLine {
    Context(String),
    Removed(String),
    Added(String),
}

#[derive(Debug, PartialEq, Eq)]
pub struct PatchError(String);

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid patch: {}", self.0)
    }
}

/// Parses the patch text into hunks. Nothing is checked against the disk here:
/// a parse error is a format error, and it must be readable as such by the model.
pub fn parse_patch(text: &str) -> Result<Patch, PatchError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i >= lines.len() || lines[i].trim() != BEGIN_PATCH {
        return Err(PatchError(format!(
            "expected \"{BEGIN_PATCH}\" on the first line"
        )));
    }
    i += 1;

    let mut hunks = Vec::new();
    let mut closed = false;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_end();
        if trimmed.trim() == END_PATCH {
            closed = true;
            break;
        }
        if let Some(path) = trimmed.strip_prefix(ADD_FILE) {
            let path = clean_path(path, i)?;
            i += 1;
            let mut body: Vec<&str> = Vec::new();
            while i < lines.len() && lines[i].starts_with('+') {
                body.push(&lines[i][1..]);
                i += 1;
            }
            let contents = if body.is_empty() {
                String::new()
            } else {
                format!("{}\n", body.join("\n"))
            };
            hunks.push(Hunk::Add { path, contents });
            continue;
        }
        if let Some(path) = trimmed.strip_prefix(DELETE_FILE) {
            hunks.push(Hunk::Delete {
                path: clean_path(path, i)?,
            });
            i += 1;
            continue;
        }
        if let Some(path) = trimmed.strip_prefix(UPDATE_FILE) {
            let path = clean_path(path, i)?;
            i += 1;
            let mut move_to = None;
            if i < lines.len()
                && let Some(dest) = lines[i].trim_end().strip_prefix(MOVE_TO)
            {
                move_to = Some(clean_path(dest, i)?);
                i += 1;
            }
            let (chunks, next) = parse_chunks(&lines, i)?;
            if chunks.is_empty() {
                return Err(PatchError(format!(
                    "\"{UPDATE_FILE}{path}\" carries no change"
                )));
            }
            i = next;
            hunks.push(Hunk::Update {
                path,
                move_to,
                chunks,
            });
            continue;
        }
        if trimmed.trim().is_empty() {
            i += 1;
            continue;
        }
        return Err(PatchError(format!(
            "line {}: expected a hunk marker (Add/Delete/Update File) or \"{END_PATCH}\", got {:?}",
            i + 1,
            line
        )));
    }
    if !closed {
        return Err(PatchError(format!("missing \"{END_PATCH}\"")));
    }
    if hunks.is_empty() {
        return Err(PatchError("no hunk between the markers".to_string()));
    }
    Ok(Patch { hunks })
}

/// Reads the chunks of an `*** Update File` hunk until the next marker.
fn parse_chunks(lines: &[&str], mut i: usize) -> Result<(Vec<Chunk>, usize), PatchError> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut current = Chunk::default();
    let mut started = false;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_end();
        if trimmed.trim() == END_PATCH
            || trimmed.starts_with(ADD_FILE)
            || trimmed.starts_with(DELETE_FILE)
            || trimmed.starts_with(UPDATE_FILE)
        {
            break;
        }
        if trimmed.trim() == END_OF_FILE {
            current.end_of_file = true;
            i += 1;
            continue;
        }
        if trimmed == "@@" || trimmed.starts_with("@@ ") {
            if started {
                chunks.push(std::mem::take(&mut current));
            }
            started = true;
            let context = trimmed.strip_prefix("@@ ").map(str::to_string);
            current = Chunk {
                context,
                ..Chunk::default()
            };
            i += 1;
            continue;
        }
        let parsed = match line.chars().next() {
            Some('+') => PatchLine::Added(line[1..].to_string()),
            Some('-') => PatchLine::Removed(line[1..].to_string()),
            Some(' ') => PatchLine::Context(line[1..].to_string()),
            // An empty line inside a chunk is a context line whose single space
            // the model dropped. Lenient like the Codex parser.
            None => PatchLine::Context(String::new()),
            Some(_) => {
                return Err(PatchError(format!(
                    "line {}: a chunk line must start with \" \", \"+\" or \"-\", got {:?}",
                    i + 1,
                    line
                )));
            }
        };
        started = true;
        current.lines.push(parsed);
        i += 1;
    }
    if started {
        chunks.push(current);
    }
    chunks.retain(|c| !c.lines.is_empty());
    Ok((chunks, i))
}

fn clean_path(raw: &str, line: usize) -> Result<String, PatchError> {
    let path = raw.trim();
    if path.is_empty() {
        return Err(PatchError(format!("line {}: empty path", line + 1)));
    }
    Ok(path.to_string())
}

// ───────────────────────── application ─────────────────────────

/// Applies the chunks to `original` and returns the new content. Pure: no disk,
/// hence testable, and the caller can decide to write only once every hunk has
/// resolved.
pub fn apply_chunks(original: &str, chunks: &[Chunk]) -> Result<String, PatchError> {
    let trailing_newline = original.ends_with('\n');
    let crlf = original.contains("\r\n");
    let mut lines: Vec<String> = if original.is_empty() {
        Vec::new()
    } else {
        let mut v: Vec<String> = original.split('\n').map(str::to_string).collect();
        if trailing_newline {
            v.pop();
        }
        v
    };

    // Chunks apply in order and never move backwards: the search of chunk N+1
    // starts where chunk N ended, which is what stops a repeated pattern from
    // making two chunks land on the same place.
    let mut cursor = 0usize;
    for (index, chunk) in chunks.iter().enumerate() {
        let old: Vec<&str> = chunk
            .lines
            .iter()
            .filter_map(|l| match l {
                PatchLine::Context(t) | PatchLine::Removed(t) => Some(t.as_str()),
                PatchLine::Added(_) => None,
            })
            .collect();
        let start = if old.is_empty() {
            // Pure insertion: without context the position is undecidable, so we
            // append at the end rather than guess.
            lines.len()
        } else {
            let anchor = chunk
                .context
                .as_deref()
                .and_then(|ctx| find_anchor(&lines, ctx, cursor))
                .unwrap_or(cursor);
            let found = seek(&lines, &old, anchor).or_else(|| {
                // The anchor may sit AFTER the change in a badly ordered patch:
                // one retry from the cursor, never from the start of the file.
                if anchor > cursor {
                    seek(&lines, &old, cursor)
                } else {
                    None
                }
            });
            match found {
                Some(pos) => pos,
                None => {
                    return Err(PatchError(format!(
                        "chunk {} does not match the current file content \
                         (context is stale; nothing was written)",
                        index + 1
                    )));
                }
            }
        };
        if chunk.end_of_file && start + old.len() != lines.len() {
            return Err(PatchError(format!(
                "chunk {} declares \"{END_OF_FILE}\" but does not end the file",
                index + 1
            )));
        }

        // Rebuild the window: a kept context line stays the ORIGINAL line (byte
        // for byte), an added line is aligned on the file's dominant terminator.
        let mut replacement: Vec<String> = Vec::new();
        let mut offset = start;
        for line in &chunk.lines {
            match line {
                PatchLine::Context(_) => {
                    if let Some(existing) = lines.get(offset) {
                        replacement.push(existing.clone());
                    }
                    offset += 1;
                }
                PatchLine::Removed(_) => {
                    offset += 1;
                }
                PatchLine::Added(text) => {
                    let core = text.strip_suffix('\r').unwrap_or(text);
                    replacement.push(if crlf {
                        format!("{core}\r")
                    } else {
                        core.to_string()
                    });
                }
            }
        }
        let end = start + old.len();
        let after = replacement.len();
        lines.splice(start..end, replacement);
        cursor = start + after;
    }

    let mut out = lines.join("\n");
    if trailing_newline && !out.is_empty() {
        out.push('\n');
    } else if !trailing_newline && out.is_empty() {
        // An emptied file stays empty, no phantom newline.
    }
    Ok(out)
}

/// First position >= `from` where `needle` matches `haystack`, comparing with
/// increasing tolerance (exact, then trailing whitespace, then full trim). Same
/// ladder as the `edit` tool, for the same reason: models rewrite whitespace.
fn seek(haystack: &[String], needle: &[&str], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    for level in [Norm::Exact, Norm::TrimEnd, Norm::Trim] {
        let last = haystack.len() - needle.len();
        for start in from..=last {
            if (0..needle.len())
                .all(|k| norm(&haystack[start + k], level) == norm(needle[k], level))
            {
                return Some(start);
            }
        }
    }
    None
}

/// Position of the `@@` anchor line, searched from `from`.
fn find_anchor(haystack: &[String], context: &str, from: usize) -> Option<usize> {
    let wanted = context.trim();
    haystack
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, line)| line.trim() == wanted)
        .map(|(i, _)| i)
}

#[derive(Clone, Copy)]
enum Norm {
    Exact,
    TrimEnd,
    Trim,
}

fn norm(line: &str, level: Norm) -> &str {
    let core = line.strip_suffix('\r').unwrap_or(line);
    match level {
        Norm::Exact => core,
        Norm::TrimEnd => core.trim_end(),
        Norm::Trim => core.trim(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(body: &str) -> Result<Patch, PatchError> {
        parse_patch(&format!("{BEGIN_PATCH}\n{body}{END_PATCH}\n"))
    }

    #[test]
    fn add_file_hunk_carries_its_contents() {
        let p = patch("*** Add File: a/b.txt\n+one\n+two\n").expect("valid patch");
        assert_eq!(
            p.hunks,
            vec![Hunk::Add {
                path: "a/b.txt".to_string(),
                contents: "one\ntwo\n".to_string(),
            }]
        );
    }

    #[test]
    fn delete_and_move_are_parsed() {
        let p = patch("*** Delete File: gone.txt\n").expect("valid patch");
        assert_eq!(
            p.hunks,
            vec![Hunk::Delete {
                path: "gone.txt".to_string()
            }]
        );
        let p = patch("*** Update File: old.rs\n*** Move to: new.rs\n@@\n-a\n+b\n")
            .expect("valid patch");
        match &p.hunks[0] {
            Hunk::Update { path, move_to, .. } => {
                assert_eq!(path, "old.rs");
                assert_eq!(move_to.as_deref(), Some("new.rs"));
            }
            other => unreachable!("expected an update hunk, got {other:?}"),
        }
    }

    #[test]
    fn missing_end_marker_is_a_parse_error() {
        let err = parse_patch("*** Begin Patch\n*** Delete File: x\n")
            .expect_err("an unterminated patch must be refused");
        assert!(err.to_string().contains(END_PATCH), "{err}");
    }

    #[test]
    fn a_body_line_without_a_prefix_is_refused() {
        let err = patch("*** Update File: a.txt\n@@\nnaked line\n")
            .expect_err("a prefix-less line must be refused");
        assert!(err.to_string().contains("must start with"), "{err}");
    }

    #[test]
    fn update_applies_in_place_and_keeps_untouched_lines() {
        let p = patch("*** Update File: a.txt\n@@\n alpha\n-beta\n+BETA\n gamma\n")
            .expect("valid patch");
        let Hunk::Update { chunks, .. } = &p.hunks[0] else {
            unreachable!("expected an update hunk")
        };
        let out = apply_chunks("alpha\nbeta\ngamma\n", chunks).expect("the chunk must apply");
        assert_eq!(out, "alpha\nBETA\ngamma\n");
    }

    #[test]
    fn stale_context_fails_without_a_partial_result() {
        let p = patch("*** Update File: a.txt\n@@\n-absent\n+new\n").expect("valid patch");
        let Hunk::Update { chunks, .. } = &p.hunks[0] else {
            unreachable!("expected an update hunk")
        };
        let err = apply_chunks("alpha\nbeta\n", chunks).expect_err("a stale context must fail");
        assert!(err.to_string().contains("stale"), "{err}");
    }

    #[test]
    fn two_chunks_never_land_on_the_same_place() {
        // The same line appears twice: each chunk must take its own occurrence.
        let p = patch("*** Update File: a.txt\n@@\n-dup\n+first\n@@\n-dup\n+second\n")
            .expect("valid patch");
        let Hunk::Update { chunks, .. } = &p.hunks[0] else {
            unreachable!("expected an update hunk")
        };
        let out = apply_chunks("dup\ndup\n", chunks).expect("both chunks must apply");
        assert_eq!(out, "first\nsecond\n");
    }

    #[test]
    fn anchor_positions_the_search() {
        let p =
            patch("*** Update File: a.txt\n@@ fn second\n-body\n+patched\n").expect("valid patch");
        let Hunk::Update { chunks, .. } = &p.hunks[0] else {
            unreachable!("expected an update hunk")
        };
        let original = "fn first\nbody\nfn second\nbody\n";
        let out = apply_chunks(original, chunks).expect("the anchored chunk must apply");
        assert_eq!(
            out, "fn first\nbody\nfn second\npatched\n",
            "the anchor must send the search past the first occurrence"
        );
    }

    #[test]
    fn crlf_files_stay_crlf() {
        let p = patch("*** Update File: a.txt\n@@\n-beta\n+BETA\n").expect("valid patch");
        let Hunk::Update { chunks, .. } = &p.hunks[0] else {
            unreachable!("expected an update hunk")
        };
        let out = apply_chunks("alpha\r\nbeta\r\ngamma\r\n", chunks).expect("the chunk must apply");
        assert_eq!(out, "alpha\r\nBETA\r\ngamma\r\n");
    }

    #[test]
    fn end_of_file_marker_must_really_end_the_file() {
        let p = patch("*** Update File: a.txt\n@@\n-alpha\n+ALPHA\n*** End of File\n")
            .expect("valid patch");
        let Hunk::Update { chunks, .. } = &p.hunks[0] else {
            unreachable!("expected an update hunk")
        };
        let err = apply_chunks("alpha\nbeta\n", chunks)
            .expect_err("a chunk that does not end the file must be refused");
        assert!(err.to_string().contains("End of File"), "{err}");
    }

    #[test]
    fn a_path_touched_twice_is_a_clash() {
        let write = |p: &str| (PathBuf::from(p), p.to_string(), String::new());
        let remove = |p: &str| (PathBuf::from(p), p.to_string());
        // A move onto a distinct path is the nominal case, not a clash.
        assert_eq!(first_clash(&[write("new.rs")], &[remove("old.rs")]), None);
        // A move onto ITSELF would write the file then delete it.
        assert_eq!(
            first_clash(&[write("a.rs")], &[remove("a.rs")]),
            Some("a.rs".to_string())
        );
        // Two hunks on the same file: the second was computed against the disk,
        // so it would silently undo the first.
        assert_eq!(
            first_clash(&[write("a.rs"), write("a.rs")], &[]),
            Some("a.rs".to_string())
        );
        // Deleted twice: the second removal fails AFTER the first succeeded.
        assert_eq!(
            first_clash(&[], &[remove("a.rs"), remove("a.rs")]),
            Some("a.rs".to_string())
        );
    }

    #[test]
    fn touched_paths_cover_the_move_destination() {
        let p = patch("*** Update File: old.rs\n*** Move to: new.rs\n@@\n-a\n+b\n")
            .expect("valid patch");
        let paths = p.touched_paths();
        assert!(paths.contains("old.rs") && paths.contains("new.rs"));
    }
}
