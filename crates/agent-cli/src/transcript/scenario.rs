//! The scenarios on disk, and the verdicts the gate renders about them
//! (US-124, US-126).
//!
//! A scenario is a directory, not a constant: its prompt, its recorded streams
//! and the transcript it must render sit side by side under
//! `crates/agent-cli/tests/transcripts/`, and the gate finds them by scanning.
//! Adding a fifth scenario is therefore a directory, never a line of gate code,
//! which is the difference between a suite that grows and a suite that gets
//! edited.
//!
//! Everything here is fallible rather than panicking: a malformed scenario is a
//! `Err(String)` naming what is malformed, so the gate reports every broken
//! directory in one run instead of dying on the first one, and so the two
//! verdicts the epic cares about (absent transcript, stale transcript) are
//! provable without producing one.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The switch that turns the gate from comparing to writing. Isolated in the
/// `regen` recipe like `PYXIS_UPDATE_SCHEMAS` and `PYXIS_UPDATE_CATALOGS`, and
/// held there by `agent-doc-gates` (US-125).
pub const UPDATE_VARIABLE: &str = "PYXIS_UPDATE_TRANSCRIPTS";

/// Named in every verdict below. A gate that says a file is stale without
/// saying how to refresh it sends the reader to the `justfile` to find out.
pub const UPDATE_COMMAND: &str =
    "PYXIS_UPDATE_TRANSCRIPTS=1 cargo test -p agent-cli --bin pyxis transcript";

/// The scenario tree, relative to the crate root.
const ROOT: &str = "tests/transcripts";

/// The transcript frozen beside its scenario.
const EXPECTED: &str = "expected.jsonl";

/// What the run is given: the prompt, and the two things that are not the
/// prompt but still decide the transcript.
const INPUT: &str = "input.json";

/// The recorded streams, played in filename order.
const SCRIPT: &str = "script";

/// The whole tree stays small enough to read in a diff. A transcript is a proof
/// a human reviews, so its size is part of its contract: past this, a scenario
/// is recording noise rather than behavior.
pub const TREE_BUDGET_BYTES: u64 = 100 * 1024;

/// Per scenario, same reasoning at the granularity a reviewer actually reads.
pub const SCENARIO_BUDGET_BYTES: u64 = 25 * 1024;

/// The answer the scripted approver gives, when a scenario declares one.
///
/// Absent from a scenario that never reaches a permission decision: the
/// registry's fail-closed default then stands, which is what the binary does
/// headless with nobody attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Allow,
    Deny,
    /// Ends the turn. The only way a test reaches `TurnState::Interrupted`
    /// through the real path: `headless::run` builds its own cancellation token
    /// and only signals it on a real Ctrl+C, which a test cannot raise without
    /// hitting every other test in the process.
    Abort,
}

/// How the run is expected to return. Not a tolerance: an interrupted turn and
/// an exhausted one both make `headless::run` return `Err`, and a scenario that
/// silently started succeeding would otherwise still render its frozen bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    Ok,
    Err,
}

/// One scenario, fully loaded.
pub struct Scenario {
    /// The directory name. It is the label every failure carries.
    pub name: String,
    pub prompt: String,
    /// Files seeded in the temporary workspace before the turn, by relative
    /// path. What makes a tool call read something instead of failing.
    pub files: BTreeMap<String, String>,
    pub approval: Option<Approval>,
    pub ending: Ending,
    /// `(name, recorded SSE)`, in the order the provider must play them.
    pub script: Vec<(String, String)>,
    /// Where the transcript is frozen. Present or not: an absent file is a
    /// verdict, not a reason to create one.
    pub expected: PathBuf,
}

/// The scenario tree, absolute.
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(ROOT)
}

/// Every scenario, in directory-name order so the gate reports in a stable one.
pub fn discover() -> Result<Vec<Scenario>, String> {
    let root = root();
    let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
        .map_err(|err| format!("{} is unreadable: {err}", root.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs.iter().map(|dir| Scenario::load(dir)).collect()
}

impl Scenario {
    fn load(dir: &Path) -> Result<Self, String> {
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{} has no name", dir.display()))?;

        let input_path = dir.join(INPUT);
        let raw = fs::read_to_string(&input_path)
            .map_err(|err| format!("scenario `{name}`: {INPUT} is unreadable: {err}"))?;
        let input: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|err| format!("scenario `{name}`: {INPUT} is not JSON: {err}"))?;

        let prompt = input
            .get("prompt")
            .and_then(|value| value.as_str())
            .ok_or_else(|| format!("scenario `{name}`: {INPUT} carries no `prompt` string"))?
            .to_string();

        let mut files = BTreeMap::new();
        if let Some(declared) = input.get("files") {
            let declared = declared
                .as_object()
                .ok_or_else(|| format!("scenario `{name}`: `files` is not an object"))?;
            for (path, content) in declared {
                let content = content
                    .as_str()
                    .ok_or_else(|| format!("scenario `{name}`: `files[{path}]` is not a string"))?;
                files.insert(path.clone(), content.to_string());
            }
        }

        let approval = match input.get("approval").and_then(|value| value.as_str()) {
            None => None,
            Some("allow") => Some(Approval::Allow),
            Some("deny") => Some(Approval::Deny),
            Some("abort") => Some(Approval::Abort),
            Some(other) => {
                return Err(format!(
                    "scenario `{name}`: unknown `approval` {other:?}, expected allow, deny or abort"
                ));
            }
        };

        let ending = match input.get("ending").and_then(|value| value.as_str()) {
            None | Some("ok") => Ending::Ok,
            Some("err") => Ending::Err,
            Some(other) => {
                return Err(format!(
                    "scenario `{name}`: unknown `ending` {other:?}, expected ok or err"
                ));
            }
        };

        let script_dir = dir.join(SCRIPT);
        let mut entries: Vec<PathBuf> = fs::read_dir(&script_dir)
            .map_err(|err| format!("scenario `{name}`: {SCRIPT}/ is unreadable: {err}"))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sse"))
            .collect();
        // Filename order IS play order, so the recorded turns are numbered.
        // Sorting a `Vec<PathBuf>` orders on the whole path, whose prefix is
        // shared, so this is the numbering and nothing else.
        entries.sort();
        if entries.is_empty() {
            return Err(format!("scenario `{name}`: {SCRIPT}/ holds no .sse stream"));
        }
        let mut script = Vec::with_capacity(entries.len());
        for path in &entries {
            let label = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let sse = fs::read_to_string(path)
                .map_err(|err| format!("scenario `{name}`: {label} is unreadable: {err}"))?;
            script.push((label, sse));
        }

        Ok(Self {
            name,
            prompt,
            files,
            approval,
            ending,
            script,
            expected: dir.join(EXPECTED),
        })
    }
}

/// The comparison itself, on the `crates/agent-app-server/tests/schemas.rs`
/// pattern: raw bytes on both sides, no `trim`, no line-ending normalization.
/// A transcript is a byte contract, and a comparison that normalizes anything
/// is a comparison that would have accepted the drift it exists to catch.
///
/// Absent and stale are two different verdicts because they have two different
/// causes: nobody ever froze this scenario, versus the code changed under one
/// that was frozen. Both name the command that fixes them.
pub fn transcript_verdict(name: &str, path: &Path, produced: &[u8]) -> Result<(), String> {
    let Ok(frozen) = fs::read(path) else {
        return Err(format!(
            "transcript missing for `{name}` at {}; regenerate with {UPDATE_COMMAND}",
            path.display()
        ));
    };
    if frozen == produced {
        return Ok(());
    }
    Err(format!(
        "{} is stale; regenerate with {UPDATE_COMMAND}\n{}",
        path.display(),
        first_difference(&frozen, produced)
    ))
}

/// The line the two sides part on, so a stale verdict is actionable without a
/// second run. Byte ranks rather than a full diff: the point is to locate the
/// drift, and printing two transcripts would bury it.
fn first_difference(frozen: &[u8], produced: &[u8]) -> String {
    let frozen_lines: Vec<&[u8]> = frozen.split(|byte| *byte == b'\n').collect();
    let produced_lines: Vec<&[u8]> = produced.split(|byte| *byte == b'\n').collect();
    for (rank, (expected, found)) in frozen_lines.iter().zip(&produced_lines).enumerate() {
        if expected != found {
            return format!(
                "  line {}:\n  frozen:   {}\n  produced: {}",
                rank + 1,
                String::from_utf8_lossy(expected),
                String::from_utf8_lossy(found)
            );
        }
    }
    format!(
        "  {} frozen line(s) against {} produced",
        frozen_lines.len(),
        produced_lines.len()
    )
}

/// The bytes a text-normalizing checkout would have changed. `.gitattributes`
/// pins the tree to `-text`, and this is what proves the pin still holds: a
/// `\r` in a frozen transcript means the file went through a conversion, and
/// every byte comparison after that is comparing the conversion.
pub fn line_ending_verdict(name: &str, path: &Path, frozen: &[u8]) -> Result<(), String> {
    if frozen.contains(&b'\r') {
        return Err(format!(
            "the transcript of `{name}` carries a \\r at {}; check .gitattributes",
            path.display()
        ));
    }
    if !frozen.ends_with(b"\n") {
        return Err(format!(
            "the transcript of `{name}` does not end on a newline at {}",
            path.display()
        ));
    }
    Ok(())
}
