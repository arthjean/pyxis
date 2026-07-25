//! TUI diagnostic trace, written to a FILE: the terminal belongs to the
//! rendering, an `eprintln!` there would corrupt the display.
//!
//! Inactive by default. `PYXIS_DEBUG_TUI=1` writes into `pyxis-tui-debug.log` under
//! the current directory (hence in the workspace, the only writable location
//! when the sandbox is active); any other value is taken as a path.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_FILE: &str = "pyxis-tui-debug.log";

fn target() -> Option<String> {
    let value = std::env::var("PYXIS_DEBUG_TUI").ok()?;
    let value = value.trim();
    match value {
        "" | "0" | "false" => None,
        "1" | "true" => Some(DEFAULT_FILE.to_string()),
        path => Some(path.to_string()),
    }
}

/// True when the trace is active: lets the caller avoid composing an
/// expensive message for nothing.
pub fn enabled() -> bool {
    target().is_some()
}

pub fn log(message: &str) {
    let Some(path) = target() else {
        return;
    };
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{millis} {message}");
    }
}
