//! `agent-parity` - regenerate or verify the Codex baseline contract matrix.
//!
//! ```text
//! agent-parity generate [--codex <path>] [--out <path>]
//! agent-parity check    [--codex <path>] [--out <path>]
//! agent-parity drift    [--codex <path>] [--out <path>]
//! agent-parity client-model-generate [--codex <path>] [--out <path>]
//! agent-parity client-model-check    [--codex <path>] [--out <path>]
//! ```
//!
//! `generate` rewrites the committed matrix; `check` fails with a readable diff
//! when the clone no longer matches it. Both refuse to run against a clone that
//! is absent or at another commit, and neither writes to the clone.
//!
//! `drift` is the upstream watch (EP-006/US-020 AC4): it reads the clone at
//! WHATEVER commit it is on and prints the difference against the committed
//! baseline. It writes nothing at all, neither to Pyxis nor to Codex, because
//! moving a baseline is a decision and not the outcome of a command: a `drift`
//! that regenerated the matrix would make the campaign follow HEAD silently,
//! which is exactly what the pinned baseline exists to prevent.
//!
//! The client/model matrix is a declarative audit record. Its commands validate
//! every source anchor and existing Pyxis proof; `client-model-generate` only
//! refreshes its deterministic fingerprint and normalized JSON representation.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::PathBuf;
use std::process::ExitCode;

use agent_parity::client_model::{
    BASELINE_COMMIT as CLIENT_MODEL_BASELINE_COMMIT,
    COMMITTED_MATRIX_PATH as CLIENT_MODEL_MATRIX_PATH, load as load_client_model_matrix,
};
use agent_parity::{
    BASELINE_COMMIT, BASELINE_PATH_ENV, COMMITTED_MATRIX_PATH, CodexBaseline,
    DEFAULT_BASELINE_PATH, ParityMatrix,
};

const USAGE: &str = "usage: agent-parity <generate|check|drift|client-model-generate|client-model-check> [--codex <path>] [--out <path>]";

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
    Drift,
    ClientModelGenerate,
    ClientModelCheck,
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mode = match args.next().as_deref() {
        Some("generate") => Mode::Generate,
        Some("check") => Mode::Check,
        Some("drift") => Mode::Drift,
        Some("client-model-generate") => Mode::ClientModelGenerate,
        Some("client-model-check") => Mode::ClientModelCheck,
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
    let mut out = PathBuf::from(
        if matches!(mode, Mode::ClientModelGenerate | Mode::ClientModelCheck) {
            CLIENT_MODEL_MATRIX_PATH
        } else {
            COMMITTED_MATRIX_PATH
        },
    );
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

    if matches!(mode, Mode::ClientModelGenerate | Mode::ClientModelCheck) {
        return run_client_model(mode, codex, out);
    }

    // `drift` is the only mode that accepts an unpinned clone: it REPORTS what
    // moved and changes nothing, so refusing on the commit would defeat it.
    let baseline = match (&mode, codex) {
        (Mode::Drift, Some(path)) => CodexBaseline::open_unpinned(path),
        (Mode::Drift, None) => CodexBaseline::open_unpinned(
            std::env::var_os(BASELINE_PATH_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_BASELINE_PATH)),
        ),
        (_, Some(path)) => CodexBaseline::open(path),
        (_, None) => CodexBaseline::from_env(),
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
        Mode::Drift => {
            let body = std::fs::read_to_string(&out)
                .map_err(|error| format!("cannot read {}: {error}", out.display()))?;
            let committed: ParityMatrix = serde_json::from_str(&body)
                .map_err(|error| format!("cannot parse {}: {error}", out.display()))?;
            println!(
                "baseline {} -> codex HEAD {}",
                committed.baseline_commit, matrix.baseline_commit
            );
            // The commit itself always differs and is already on the line above.
            // What matters is whether the CONTRACT moved, so the reference is
            // re-stamped before the comparison: reporting "1 difference" for a
            // clone that merely advanced would train a reader to ignore the
            // report.
            let mut reference = committed.clone();
            reference.baseline_commit = matrix.baseline_commit.clone();
            let differences = reference.diff(&matrix);
            if differences.is_empty() {
                println!("no contract drift: HEAD still matches the pinned baseline");
                return Ok(());
            }
            for line in &differences {
                println!("  {line}");
            }
            println!(
                "\n{} contract difference(s). Moving the baseline is a DECISION: \
                 update BASELINE_COMMIT, run `agent-parity generate`, and read the diff.",
                differences.len()
            );
            // Non-zero so a scheduled run signals rather than scrolls past. The
            // report went to stdout, the reason goes to stderr through `main`.
            Err("upstream contract drift detected".to_string())
        }
        Mode::ClientModelGenerate | Mode::ClientModelCheck => unreachable!(),
    }
}

fn run_client_model(mode: Mode, codex: Option<PathBuf>, out: PathBuf) -> Result<(), String> {
    let path = codex.unwrap_or_else(|| {
        std::env::var_os(BASELINE_PATH_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_BASELINE_PATH))
    });
    let baseline = CodexBaseline::open_at_commit(path, CLIENT_MODEL_BASELINE_COMMIT)
        .map_err(|error| error.to_string())?;
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source = if matches!(mode, Mode::ClientModelGenerate) && !out.exists() {
        repository_root.join(CLIENT_MODEL_MATRIX_PATH)
    } else {
        out.clone()
    };
    let mut matrix = load_client_model_matrix(&source)?;

    match mode {
        Mode::ClientModelGenerate => {
            matrix.refresh_fingerprint();
            matrix.validate(&repository_root)?;
            matrix.verify_baseline(&baseline)?;
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
            }
            std::fs::write(&out, matrix.to_pretty_json())
                .map_err(|error| format!("cannot write {}: {error}", out.display()))?;
            println!(
                "client/model matrix validated and written to {} (commit {}, fingerprint {})",
                out.display(),
                matrix.baseline_commit,
                matrix.fingerprint
            );
            Ok(())
        }
        Mode::ClientModelCheck => {
            matrix.validate(&repository_root)?;
            matrix.verify_baseline(&baseline)?;
            println!(
                "client/model matrix is valid at {} (commit {}, fingerprint {})",
                out.display(),
                matrix.baseline_commit,
                matrix.fingerprint
            );
            Ok(())
        }
        Mode::Generate | Mode::Check | Mode::Drift => unreachable!(),
    }
}
