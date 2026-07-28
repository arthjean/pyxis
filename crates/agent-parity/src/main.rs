//! `agent-parity` - regenerate or verify the Codex baseline contract matrix.
//!
//! ```text
//! agent-parity generate [--codex <path>] [--out <path>]
//! agent-parity check    [--codex <path>] [--out <path>]
//! ```
//!
//! `generate` rewrites the committed matrix; `check` fails with a readable diff
//! when the clone no longer matches it. Both refuse to run against a clone that
//! is absent or at another commit, and neither writes to the clone.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::PathBuf;
use std::process::ExitCode;

use agent_parity::{
    BASELINE_COMMIT, BASELINE_PATH_ENV, COMMITTED_MATRIX_PATH, CodexBaseline,
    DEFAULT_BASELINE_PATH, ParityMatrix,
};

const USAGE: &str = "usage: agent-parity <generate|check> [--codex <path>] [--out <path>]";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("agent-parity: {message}");
            ExitCode::FAILURE
        }
    }
}

enum Mode {
    Generate,
    Check,
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mode = match args.next().as_deref() {
        Some("generate") => Mode::Generate,
        Some("check") => Mode::Check,
        Some("--help" | "-h") => {
            println!("{USAGE}");
            println!("baseline commit: {BASELINE_COMMIT}");
            println!("baseline path:   ${BASELINE_PATH_ENV} or {DEFAULT_BASELINE_PATH}");
            return Ok(());
        }
        Some(other) => return Err(format!("unknown command {other}\n{USAGE}")),
        None => return Err(format!("missing command\n{USAGE}")),
    };

    let mut codex: Option<PathBuf> = None;
    let mut out = PathBuf::from(COMMITTED_MATRIX_PATH);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--codex" => {
                codex = Some(PathBuf::from(
                    args.next().ok_or("--codex expects a path".to_string())?,
                ));
            }
            "--out" => {
                out = PathBuf::from(args.next().ok_or("--out expects a path".to_string())?);
            }
            other => return Err(format!("unknown flag {other}\n{USAGE}")),
        }
    }

    let baseline = match codex {
        Some(path) => CodexBaseline::open(path),
        None => CodexBaseline::from_env(),
    }
    .map_err(|error| error.to_string())?;
    let matrix = ParityMatrix::extract(&baseline).map_err(|error| error.to_string())?;

    match mode {
        Mode::Generate => {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
            }
            std::fs::write(&out, matrix.to_pretty_json())
                .map_err(|error| format!("cannot write {}: {error}", out.display()))?;
            println!(
                "matrix written to {} (commit {}, fingerprint {})",
                out.display(),
                matrix.baseline_commit,
                matrix.fingerprint
            );
            Ok(())
        }
        Mode::Check => {
            let body = std::fs::read_to_string(&out)
                .map_err(|error| format!("cannot read {}: {error}", out.display()))?;
            let committed: ParityMatrix = serde_json::from_str(&body)
                .map_err(|error| format!("cannot parse {}: {error}", out.display()))?;
            let differences = committed.diff(&matrix);
            if differences.is_empty() {
                println!(
                    "matrix matches {} (commit {}, fingerprint {})",
                    out.display(),
                    matrix.baseline_commit,
                    matrix.fingerprint
                );
                return Ok(());
            }
            let detail = differences.join("\n  ");
            Err(format!(
                "{} is stale, regenerate with `agent-parity generate`:\n  {detail}",
                out.display()
            ))
        }
    }
}
