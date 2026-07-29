//! The one parity scenario that talks to OpenAI (US-020 AC2/AC3).
//!
//! Everything else about parity is proven offline, by fixtures and contract
//! tests. This scenario proves the remaining thing fixtures cannot: that a real
//! `gpt-5.6-sol` turn, on a real ChatGPT subscription, runs a Code Mode cell to
//! a terminal result and leaves a resumable transcript.
//!
//! **It is opt-in and it cannot manufacture a success.** A green test here means
//! nothing on its own: the verdict is a FILE, `target/parity/live-verdict.json`,
//! carrying `skipped`, `passed` or `external_error`. Without
//! `PYXIS_LIVE_PARITY=1` or without a local credential the run is `skipped`,
//! which is a hole in the proof and says so; an OpenAI failure is reported with
//! its exact error AND fails the test, because the maintainer explicitly asked
//! for a live run and did not get one.
//!
//! ```bash
//! PYXIS_LIVE_PARITY=1 cargo test -p agent-cli --test live_parity_sol -- --nocapture
//! ```
//!
//! The recipe and how to read the verdict: `docs/parity/offline-suite.md`.
// `panic!` is the reporting mechanism here, not an escape hatch: the verdict
// file records the outcome first, and the panic is what makes an explicitly
// requested live run FAIL instead of passing on a hole in the proof.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// Opt-in switch. Absent = the scenario does not run and does not pretend to.
const OPT_IN: &str = "PYXIS_LIVE_PARITY";
/// The frontier model the campaign exists for: `code_mode_only`, multi-agent v2.
const MODEL: &str = "gpt-5.6-sol";
/// A prompt whose only route to an answer is a cell, since a `code_mode_only`
/// model has no direct tool. Deliberately arithmetic: the answer is checkable
/// without reading the model's prose.
///
/// The nested tools are ruled OUT on purpose. The first version of this prompt
/// said only "by running JavaScript" and the model reached for
/// `tools.bash("node -e ...")`: the bash output is untrusted, the taint window
/// turns the next call into a confirmation, and a headless run has nobody to
/// confirm it, so the scenario failed on a fail-closed refusal that was working
/// exactly as designed. What this scenario must exercise is the CELL, so the
/// prompt keeps the computation inside the isolate.
const PROMPT: &str = "Call the exec tool once. Inside the cell, compute 6 * 7 in plain JavaScript \
     and emit the result with text(). Do not call any nested tool, and do not answer from memory. \
     Then reply with the number alone.";
/// A live turn is bounded: a scenario that hangs is a scenario nobody runs.
const BUDGET: std::time::Duration = std::time::Duration::from_secs(180);

fn verdict_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/parity/live-verdict.json")
}

/// Writes the verdict and echoes it, so a `-- --nocapture` run shows the same
/// thing the file holds.
fn record(status: &str, detail: &str) {
    let path = verdict_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = serde_json::json!({
        "scenario": "live-sol-code-mode",
        "model": MODEL,
        "status": status,
        "detail": detail,
    });
    let rendered = serde_json::to_string_pretty(&body).unwrap_or_default();
    let _ = std::fs::write(&path, format!("{rendered}\n"));
    println!("[parity live] {status}: {detail}");
    println!("[parity live] verdict written to {}", path.display());
}

/// Throwaway workspace: the run writes a session file there and nothing else.
struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("pyxis-live-parity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the temporary workspace is created");
        Self(path.canonicalize().unwrap_or(path))
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The session file the run left behind, if any.
fn session_file(workspace: &Path) -> Option<PathBuf> {
    std::fs::read_dir(workspace.join(".pyxis/sessions"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
}

#[test]
fn a_live_sol_turn_runs_a_code_mode_cell_and_stays_resumable() {
    if std::env::var_os(OPT_IN).is_none() {
        record(
            "skipped",
            &format!("{OPT_IN} is not set: the live scenario was not requested"),
        );
        return;
    }
    // A credential is a precondition of the scenario, not a result of it: no
    // credential means the parity claim is untested, never proven.
    match agent_auth::store::load(agent_provider::KEYRING_ACCOUNT) {
        Ok(Some(_)) => {}
        Ok(None) => {
            record(
                "skipped",
                "no ChatGPT subscription credential in the keyring: run `pyxis` and /login first",
            );
            return;
        }
        Err(error) => {
            record("skipped", &format!("keyring unreadable: {error}"));
            return;
        }
    }

    let workspace = TempWorkspace::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pyxis"))
        .current_dir(&workspace.0)
        // `--sandbox full-access` would be a widening; the default workspace
        // policy is exactly what a real run of this prompt gets.
        .args([
            "--model",
            MODEL,
            "--permission-mode",
            "dont-ask",
            "--output-format",
            "json",
            "-p",
            PROMPT,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the pyxis binary runs");

    let deadline = std::time::Instant::now() + BUDGET;
    loop {
        match child.try_wait().expect("the child is waitable") {
            Some(_) => break,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let detail = format!("the live turn exceeded {BUDGET:?} and was killed");
                record("external_error", &detail);
                panic!("{detail}");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(250)),
        }
    }
    let output = child
        .wait_with_output()
        .expect("the child output is readable");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        // The exact external error, unedited. Converting it into a pass is the
        // one thing AC3 forbids.
        let detail = format!(
            "pyxis exited {:?}; stderr: {}",
            output.status.code(),
            stderr.trim()
        );
        record("external_error", &detail);
        panic!("{detail}");
    }

    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();

    // 1. At least one Code Mode cell. A `code_mode_only` model has no other way
    // to run anything, so its absence means the turn answered from memory and
    // proves nothing about Code Mode.
    let cells = lines
        .iter()
        .filter(|line| line["type"] == "tool_call" && line["data"]["name"] == "exec")
        .count();
    // 2. One terminal result, clean.
    let summary = lines
        .iter()
        .find(|line| line["type"] == "run_summary")
        .cloned();
    // 3. A transcript that can be reopened.
    let session = session_file(&workspace.0);

    let failures: Vec<String> = [
        (cells == 0).then(|| "no `exec` cell was dispatched".to_string()),
        summary
            .as_ref()
            .filter(|summary| summary["data"]["end"] == "end_turn")
            .is_none()
            .then(|| match &summary {
                Some(summary) => format!(
                    "the turn did not complete: end={}, detail={}",
                    summary["data"]["end"], summary["data"]["end_detail"]
                ),
                None => "the run emitted no summary line".to_string(),
            }),
        session
            .as_ref()
            .is_none()
            .then(|| "no session file was written: the transcript is not resumable".to_string()),
    ]
    .into_iter()
    .flatten()
    .collect();

    if !failures.is_empty() {
        let detail = failures.join("; ");
        record("external_error", &detail);
        panic!("{detail}");
    }

    let session = session.expect("checked above");
    let transcript = std::fs::read_to_string(&session).expect("the session file is readable");
    assert!(
        transcript.contains("thread_event"),
        "the session carries no durable orchestration event: {}",
        session.display()
    );

    record(
        "passed",
        &format!(
            "{cells} code mode cell(s), turn completed, transcript at {}",
            session.display()
        ),
    );
}
