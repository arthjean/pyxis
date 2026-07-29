//! Process observability (US-020): panic net and structured trace.
//!
//! Division of labour imposed by FR-15: the crates EMIT through the `tracing`
//! facade and never touch a process output; this module, which belongs to the
//! binary, is the only place that installs a subscriber and the only one that
//! decides where a diagnostic is written.
//!
//! Two constraints shape everything here:
//!
//! - **Landlock.** The confinement is irreversible and applied before the Tokio
//!   runtime. A file that must stay writable afterwards has to exist BEFORE, and
//!   be granted explicitly, exactly like `~/.pyxis/settings.toml`. Hence
//!   `prepare()` is called at the very top of `main` and returns the paths that
//!   the sandbox must grant.
//! - **Silence by default.** Without `PYXIS_LOG` no subscriber is installed, so
//!   every `tracing` macro of the workspace collapses to an atomic level check
//!   and produces not one byte (AC4).
//!
//! **Remote observability (US-019 AC4 of `prd-parite-totale-codex-cli`).** There
//! is none, on purpose, and the absence is what the criterion asks for: Pyxis
//! must open zero observability connection when OTLP is unconfigured, which a
//! build carrying no exporter satisfies by construction rather than by a
//! runtime guard nobody re-checks. The trace has exactly ONE sink, a local file
//! under the state root, so a missing or unreachable collector cannot cost a
//! local diagnostic. `no_observability_exporter_is_linked_into_the_build`
//! enforces it against the resolved dependency graph: adding an exporter later
//! is then a deliberate change to that test, with the opt-in it implies.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::Level;

/// Environment variable enabling the trace, and its level: `error`, `warn`,
/// `info`, `debug`, `trace`. Absent or unreadable = no subscriber.
const LOG_ENV: &str = "PYXIS_LOG";
/// Directory of the diagnostics, outside the workspace (AC2).
const LOG_DIR: &str = "logs";
/// Panic reports of every run, appended. Pre-created empty at each startup,
/// because a panic cannot open a new file once Landlock is in place.
const PANIC_FILE: &str = "panic.log";

/// Paths prepared before the sandbox. `main` grants them in write, then installs
/// the hook and the subscriber.
#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    /// Where a panic report goes. `None` = no writable home directory.
    pub panic_log: Option<PathBuf>,
    /// Where the structured trace goes when `PYXIS_LOG` asks for one.
    pub trace_log: Option<PathBuf>,
    /// Level asked for, `None` when the trace stays off.
    pub level: Option<Level>,
    /// Diagnostics of the preparation itself. Never fatal; `main` writes them.
    pub warnings: Vec<String>,
}

impl Diagnostics {
    /// Files that must stay writable once the sandbox is enforced.
    pub fn writable_files(&self) -> Vec<&Path> {
        [self.panic_log.as_deref(), self.trace_log.as_deref()]
            .into_iter()
            .flatten()
            .collect()
    }
}

/// Reads the requested level. Anything unrecognized leaves the trace off rather
/// than guessing a verbosity: a wrong guess either floods a session or hides what
/// was asked for.
pub fn requested_level(raw: Option<&str>) -> Option<Level> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "off" | "false" => None,
        "error" => Some(Level::ERROR),
        "warn" | "warning" => Some(Level::WARN),
        "info" | "1" | "true" => Some(Level::INFO),
        "debug" => Some(Level::DEBUG),
        "trace" | "all" => Some(Level::TRACE),
        _ => None,
    }
}

/// Prepares the diagnostic files under the Pyxis state root (`~/.pyxis`, or
/// `$PYXIS_HOME`). Called at the very top of `main`, before the sandbox. Creates
/// nothing beyond an empty file and a directory, and never fails the startup
/// (FR-14 spirit): without a state root, a panic still reports on stderr, only its
/// persistence is lost.
pub fn prepare(home: Option<&Path>) -> Diagnostics {
    let raw = std::env::var(LOG_ENV).ok();
    let level = requested_level(raw.as_deref());
    let mut diagnostics = Diagnostics {
        level,
        ..Diagnostics::default()
    };
    if raw.is_some() && level.is_none() {
        // Asking for a trace and getting none silently would be the worst of the
        // two: reported, once, by the binary.
        diagnostics.warnings.push(format!(
            "{LOG_ENV}=\"{}\" unrecognized: trace off (error|warn|info|debug|trace)",
            raw.unwrap_or_default()
        ));
    }
    let Some(dir) = home.map(|home| home.join(LOG_DIR)) else {
        if level.is_some() {
            diagnostics
                .warnings
                .push("no state root: no trace file, no persisted panic report".to_string());
        }
        return diagnostics;
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        diagnostics
            .warnings
            .push(format!("{}: {err}", dir.display()));
        return diagnostics;
    }

    let panic_log = dir.join(PANIC_FILE);
    match touch(&panic_log) {
        Ok(()) => diagnostics.panic_log = Some(panic_log),
        Err(err) => diagnostics
            .warnings
            .push(format!("{}: {err}", panic_log.display())),
    }
    if level.is_some() {
        // One file per run: a trace is read after the fact, and mixing two
        // sessions in one file makes it unreadable.
        let trace_log = dir.join(format!("trace-{}.log", stamp()));
        match touch(&trace_log) {
            Ok(()) => diagnostics.trace_log = Some(trace_log),
            Err(err) => diagnostics
                .warnings
                .push(format!("{}: {err}", trace_log.display())),
        }
    }
    diagnostics
}

/// Installs the subscriber, ONCE, for the whole process. No level asked = nothing
/// installed, hence zero cost on every emission point of the workspace.
///
/// Returns the file actually written to, for the startup message.
pub fn install_tracing(diagnostics: &Diagnostics) -> Option<PathBuf> {
    let level = diagnostics.level?;
    let path = diagnostics.trace_log.as_ref()?;
    let subscriber = build_subscriber(level, path)?;
    // A second install would mean two subscribers fighting over the same file:
    // failing here is the correct outcome, and it is not worth a startup failure.
    tracing::subscriber::set_global_default(subscriber).ok()?;
    Some(path.clone())
}

/// The subscriber itself, separated from its global installation so a test can
/// exercise it on one thread: `set_global_default` succeeds once per process and
/// is not something a test can own.
fn build_subscriber(
    level: Level,
    path: &Path,
) -> Option<impl tracing::Subscriber + Send + Sync + 'static> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    Some(
        tracing_subscriber::fmt()
            .with_max_level(level)
            // No ANSI: this goes into a file that a human greps later.
            .with_ansi(false)
            .with_target(true)
            .with_writer(std::sync::Mutex::new(file))
            .finish(),
    )
}

/// Installs the panic net (AC1/AC2). Order matters and is the whole point:
/// restore the terminal FIRST, so the message lands on a usable screen, then
/// persist, then tell the user where to look.
pub fn install_panic_hook(panic_log: Option<PathBuf>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // 1. The terminal comes back before anything is printed. No-op when no
        // interactive session is running.
        agent_tui::restore();

        let report = format_report(
            payload_message(info.payload()),
            info.location().map(ToString::to_string).as_deref(),
        );
        // 2. Persistence outside the workspace, when a writable path was prepared
        // before the sandbox.
        let written = panic_log.as_deref().and_then(|path| append(path, &report));
        // 3. The previous hook runs first: it owns `RUST_BACKTRACE`, which we do
        // not want to reimplement, and its details belong ABOVE our conclusion.
        previous(info);
        // 4. The last line is the actionable one. The binary is the only crate
        // allowed to write here (FR-15).
        match written {
            Some(path) => eprintln!(
                "\nPyxis hit a fatal error. Trace written to {}.",
                path.display()
            ),
            None => eprintln!("\nPyxis hit a fatal error (no trace file available).\n{report}"),
        }
    }));
}

/// Report of a panic: what happened, where, and when.
pub fn format_report(message: &str, location: Option<&str>) -> String {
    let mut report = String::new();
    let _ = writeln!(report, "--- pyxis panic @ {} ---", stamp());
    let _ = writeln!(report, "version: {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(report, "location: {}", location.unwrap_or("unknown"));
    let _ = writeln!(report, "message: {message}");
    report
}

/// Message of a panic payload, whatever shape it takes.
fn payload_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic without a message")
}

/// Appends a report and returns the path on success. Infallible for the caller:
/// a panic report that cannot be written must not create a second panic.
fn append(path: &Path, report: &str) -> Option<PathBuf> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    file.write_all(report.as_bytes()).ok()?;
    Some(path.to_path_buf())
}

/// Creates the file (and its parents) when missing, without truncating it: the
/// write right granted by Landlock only applies to an already openable path.
fn touch(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(|_| ())
}

fn stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempHome(PathBuf);

    impl TempHome {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("pyxis-obs-{}-{tag}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// AC4: no variable, nothing to install, and no trace file created.
    #[test]
    fn without_the_variable_no_trace_is_prepared() {
        let home = TempHome::new("silent");
        let diagnostics = prepare(Some(&home.0));

        assert!(diagnostics.level.is_none());
        assert!(diagnostics.trace_log.is_none());
        assert!(install_tracing(&diagnostics).is_none());
        assert!(
            diagnostics.warnings.is_empty(),
            "{:?}",
            diagnostics.warnings
        );
        // The panic net, in contrast, is always armed: it costs one empty file.
        assert!(diagnostics.panic_log.is_some());
    }

    /// AC3: a level asked for gives a file, outside the workspace, granted in
    /// write before the sandbox.
    #[test]
    fn a_requested_level_prepares_a_granted_trace_file() {
        let home = TempHome::new("level");
        let mut diagnostics = Diagnostics {
            level: Some(Level::DEBUG),
            ..Diagnostics::default()
        };
        // `prepare` reads the environment, which is process-wide: we exercise the
        // same path by asking for the level directly.
        let dir = home.0.join(LOG_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let trace = dir.join("trace-test.log");
        touch(&trace).unwrap();
        diagnostics.trace_log = Some(trace.clone());
        diagnostics.panic_log = Some(dir.join(PANIC_FILE));
        touch(diagnostics.panic_log.as_deref().unwrap()).unwrap();

        let granted = diagnostics.writable_files();
        assert_eq!(granted.len(), 2, "{granted:?}");
        assert!(granted.iter().all(|p| p.exists()), "{granted:?}");
        assert!(granted.iter().all(|p| p.starts_with(&home.0)));
    }

    /// AC3: the subscriber the binary builds does collect what the crates emit,
    /// into the file prepared before the sandbox, and it honors the level asked
    /// for (AC6: nothing above `info` here).
    #[test]
    fn the_subscriber_writes_the_events_of_its_level() {
        let home = TempHome::new("subscriber");
        let path = home.0.join(LOG_DIR).join("trace-test.log");
        touch(&path).unwrap();

        {
            let subscriber = build_subscriber(Level::INFO, &path).unwrap();
            let _guard = tracing::subscriber::set_default(subscriber);
            tracing::info!(target: "pyxis::test", answer = 42, "collected");
            tracing::debug!(target: "pyxis::test", "below the requested level");
        }

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("collected"), "{written}");
        assert!(written.contains("answer=42"), "{written}");
        assert!(written.contains("pyxis::test"), "{written}");
        assert!(
            !written.contains("below the requested level"),
            "un événement sous le niveau demandé ne doit pas être écrit: {written}"
        );
    }

    #[test]
    fn levels_are_read_strictly() {
        assert_eq!(requested_level(Some("debug")), Some(Level::DEBUG));
        assert_eq!(requested_level(Some("  TRACE ")), Some(Level::TRACE));
        assert_eq!(requested_level(Some("info")), Some(Level::INFO));
        assert_eq!(requested_level(None), None);
        assert_eq!(requested_level(Some("")), None);
        assert_eq!(requested_level(Some("off")), None);
        // Unknown value: off, and `prepare` says so rather than guessing.
        assert_eq!(requested_level(Some("verbose")), None);
    }

    /// AC2: the report names the message, the location and the version, and it is
    /// appended so a second panic never erases the first.
    #[test]
    fn panic_reports_accumulate_with_message_and_location() {
        let home = TempHome::new("panic");
        let path = home.0.join(LOG_DIR).join(PANIC_FILE);
        touch(&path).unwrap();

        append(&path, &format_report("boum", Some("src/main.rs:12:3"))).unwrap();
        append(&path, &format_report("re-boum", None)).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("message: boum"), "{written}");
        assert!(written.contains("location: src/main.rs:12:3"), "{written}");
        assert!(written.contains("message: re-boum"), "{written}");
        assert!(written.contains("location: unknown"), "{written}");
        assert_eq!(written.matches("pyxis panic @").count(), 2, "{written}");
    }

    /// AC1/AC2 end to end: with the hook installed, a real panic leaves a report
    /// naming its message and its location, and the process keeps running.
    ///
    /// The hook is process-wide, so it is removed right after: `take_hook`
    /// reinstalls the default one, and the rest of the test binary keeps its
    /// normal failure output.
    #[test]
    fn the_installed_hook_persists_a_real_panic() {
        let home = TempHome::new("hook");
        let path = home.0.join(LOG_DIR).join(PANIC_FILE);
        touch(&path).unwrap();

        install_panic_hook(Some(path.clone()));
        let outcome = std::panic::catch_unwind(|| {
            // A panic without the `panic!` macro, which the workspace denies. The
            // value comes from the environment so that the compiler cannot see the
            // `None` and rewrite the intent away.
            let missing = std::env::var_os("PYXIS_A_VARIABLE_NOBODY_SETS").map(|_| 0_u8);
            missing.unwrap()
        });
        let _ = std::panic::take_hook();

        assert!(outcome.is_err(), "le panic doit bien se produire");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("pyxis panic @"), "{written}");
        assert!(written.contains("observability.rs"), "{written}");
        assert!(
            written.contains("called `Option::unwrap()`"),
            "le message du panic doit être conservé: {written}"
        );
        // The terminal was not interactive here: nothing to restore, and the
        // sequence must not have been emitted (proved in `agent-tui::term`).
        assert!(!agent_tui::is_active());
    }

    /// Every source file of a crate, recursively.
    fn rust_sources(root: &Path) -> Vec<(PathBuf, String)> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(rust_sources(&path));
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                found.push((path, content));
            }
        }
        found
    }

    /// Confidentiality NFR / US-019 AC4: a trace at `error`, `warn`, `info` or
    /// `debug` carries identifiers and durations, never content. Content is
    /// allowed at `trace` alone.
    ///
    /// Checked mechanically because the failure mode is a one-line addition
    /// nobody reviews twice: a `text = %submission.text` added to an existing
    /// span leaks a whole prompt into a file the user is told is safe to share.
    #[test]
    fn no_trace_below_the_trace_level_carries_content() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let roots = [
            manifest.join("src"),
            manifest.join("..").join("agent-runtime").join("src"),
        ];
        // Field names that would carry a prompt, a tool result or a message.
        const BANNED: &[&str] = &[
            "prompt =",
            "prompt=",
            "text =",
            "text=",
            "content =",
            "content=",
            ".text()",
            "submission.text",
            "message =",
            "message=",
        ];
        let mut offenders: Vec<String> = Vec::new();
        let mut inspected = 0usize;
        for (path, source) in roots.iter().flat_map(|root| rust_sources(root)) {
            for level in ["error!", "warn!", "info!", "debug!"] {
                let needle = format!("tracing::{level}");
                let mut from = 0usize;
                while let Some(found) = source[from..].find(&needle) {
                    let start = from + found;
                    // The macro invocation ends at the first `);` that closes
                    // it. Good enough: these call sites are one statement each.
                    let end = source[start..]
                        .find(");")
                        .map_or(source.len(), |offset| start + offset);
                    let call = &source[start..end];
                    inspected += 1;
                    for banned in BANNED {
                        if call.contains(banned) {
                            offenders.push(format!(
                                "{}: tracing::{level} carries `{banned}`",
                                path.display()
                            ));
                        }
                    }
                    from = end.max(start + needle.len());
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "content must stay out of the traces a user is told to share:\n{}",
            offenders.join("\n")
        );
        // A scan that found nothing to scan proves nothing.
        assert!(
            inspected >= 10,
            "only {inspected} trace call sites were inspected: the scan is not reaching the sources"
        );
    }

    /// US-019 AC4: no observability EXPORTER is linked in, so an unconfigured or
    /// unreachable OTLP collector cannot cost a connection nor a local error.
    ///
    /// Checked against the resolved graph rather than the manifests: a
    /// transitive dependency would open sockets just as well as a direct one,
    /// and the lock file is the only place that lists both.
    #[test]
    fn no_observability_exporter_is_linked_into_the_build() {
        let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
        let body = std::fs::read_to_string(&lock).unwrap_or_default();
        assert!(
            !body.is_empty(),
            "{} must be readable for this check to mean anything",
            lock.display()
        );
        // Crate names, as they appear in `name = "..."` entries of the lock.
        const EXPORTERS: &[&str] = &[
            "opentelemetry",
            "opentelemetry_sdk",
            "opentelemetry-otlp",
            "opentelemetry-proto",
            "tracing-opentelemetry",
            "sentry",
            "datadog-tracing",
            "minitrace",
        ];
        let linked: Vec<&str> = EXPORTERS
            .iter()
            .copied()
            .filter(|crate_name| body.contains(&format!("name = \"{crate_name}\"")))
            .collect();
        assert!(
            linked.is_empty(),
            "a remote observability exporter entered the graph ({linked:?}); \
             remote export is opt-in by DESIGN and needs an explicit decision"
        );
    }

    /// The only sink of the trace is a file under the state root. Proven by
    /// writing through the real subscriber and reading the bytes back: a
    /// subscriber that also shipped events elsewhere would still pass a manifest
    /// scan, so the two tests are not the same check.
    #[test]
    fn the_trace_has_exactly_one_sink_and_it_is_a_local_file() {
        let home = TempHome::new("sink");
        let path = home.0.join(LOG_DIR).join("trace-test.log");
        touch(&path).unwrap();

        {
            let subscriber = build_subscriber(Level::DEBUG, &path).unwrap();
            let _guard = tracing::subscriber::set_default(subscriber);
            tracing::debug!(target: "pyxis::test", "one sink");
        }

        assert!(
            std::fs::read_to_string(&path).unwrap().contains("one sink"),
            "the file sink received the event"
        );
        // The trace file is inside the state root, hence inside what the sandbox
        // granted: nothing left the machine to carry it.
        assert!(path.starts_with(&home.0));
    }

    /// Without a home directory nothing is prepared, and the startup goes on.
    #[test]
    fn a_missing_home_is_not_fatal() {
        let diagnostics = prepare(None);
        assert!(diagnostics.panic_log.is_none());
        assert!(diagnostics.trace_log.is_none());
        assert!(diagnostics.writable_files().is_empty());
    }
}
