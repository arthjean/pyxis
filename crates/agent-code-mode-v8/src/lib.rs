//! V8 engine for Code Mode (EP-002, US-007).
//!
//! One isolate per CELL, one dedicated OS thread per cell, one store per
//! session. That shape comes from the Codex baseline and from the US-005
//! measurements: creating an isolate costs ~4 ms at P95, which buys three
//! properties a shared isolate cannot give. Cells run concurrently without
//! cooperative interleaving inside a single-threaded isolate; each cell owns
//! its own CPU, heap and native quota; and a cell that wrecks its isolate takes
//! nothing else down with it. Cross-cell state is therefore explicit: `store`
//! and `load` read and write the session store, never the global object.
//!
//! What this crate does NOT provide is a capability sandbox. An isolate
//! separates JavaScript state and bounds three resources; it does not decide
//! what a tool may do. Every nested call still goes through the Pyxis
//! permission pipeline (US-008).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod engine;
mod globals;

pub use engine::{EngineLimits, IsolateEngine};

use std::sync::OnceLock;

/// Whether V8 may generate machine code at runtime. V8 reads this ONCE per
/// process, so it cannot be chosen per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum V8JitMode {
    #[default]
    Enabled,
    Jitless,
}

impl V8JitMode {
    fn label(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Jitless => "disabled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum V8Error {
    #[error("v8 is already initialized with JIT {active}, cannot start a session with JIT {asked}")]
    JitModeLocked {
        active: &'static str,
        asked: &'static str,
    },
    #[error("v8 initialization failed: {detail}")]
    Initialization { detail: String },
}

struct Platform {
    _platform: v8::SharedRef<v8::Platform>,
    jit: V8JitMode,
}

static PLATFORM: OnceLock<Result<Platform, String>> = OnceLock::new();

/// Initializes the process-wide V8 platform in `jit` mode.
///
/// The first call wins. A later call asking for the other mode is REFUSED, and
/// refused here, before a session exists: silently running under the first mode
/// would make the JIT policy of a session unknowable (US-007 AC2).
pub fn initialize_v8(jit: V8JitMode) -> Result<(), V8Error> {
    match PLATFORM.get_or_init(|| initialize_once(jit)) {
        Ok(platform) if platform.jit == jit => Ok(()),
        Ok(platform) => Err(V8Error::JitModeLocked {
            active: platform.jit.label(),
            asked: jit.label(),
        }),
        Err(detail) => Err(V8Error::Initialization {
            detail: detail.clone(),
        }),
    }
}

/// Initializes V8 with the default JIT policy if nothing has done so yet.
pub(crate) fn ensure_initialized() -> Result<(), V8Error> {
    match PLATFORM.get_or_init(|| initialize_once(V8JitMode::default())) {
        Ok(_) => Ok(()),
        Err(detail) => Err(V8Error::Initialization {
            detail: detail.clone(),
        }),
    }
}

/// JIT mode actually in force, or `None` when V8 has not been initialized.
pub fn active_jit_mode() -> Option<V8JitMode> {
    match PLATFORM.get() {
        Some(Ok(platform)) => Some(platform.jit),
        _ => None,
    }
}

fn initialize_once(jit: V8JitMode) -> Result<Platform, String> {
    v8::icu::set_common_data_77(deno_core_icudata::ICU_DATA)
        .map_err(|code| format!("icu data rejected by v8 (code {code})"))?;
    if jit == V8JitMode::Jitless {
        v8::V8::set_flags_from_string("--jitless");
    }
    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform.clone());
    v8::V8::initialize();
    Ok(Platform {
        _platform: platform,
        jit,
    })
}
