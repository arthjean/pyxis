//! US-005: V8 envelope and security boundary, measured on the real toolchain.
//!
//! The question this spike settles is NOT "does rusty_v8 compile", it is
//! whether an in-process isolate can be given a bounded envelope: interrupted
//! from the outside in under a second, capped on V8 heap AND on native memory
//! with the two breaches distinguishable, and joined on shutdown. A threshold
//! missed here blocks US-006 with a process-owned recommendation instead of
//! being papered over by an undocumented fallback (US-005 AC4).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod host;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use host::{CellFailure, HeapSnapshot, IsolateHost, IsolateLimits};
use serde::Serialize;

/// PRD security budget: an interruption must land in under a second.
pub const MAX_INTERRUPT: Duration = Duration::from_secs(1);
/// PRD performance budget: a warm cell starts in under 100 ms at P95.
pub const MAX_WARM_CELL_P95: Duration = Duration::from_millis(100);
/// PRD performance budget: a cold session is ready in under 1 s at P95.
pub const MAX_COLD_SESSION_P95: Duration = Duration::from_millis(1000);

/// One measured dimension plus the threshold it is judged against.
#[derive(Debug, Clone, Serialize)]
pub struct Threshold {
    pub name: &'static str,
    pub measured: String,
    pub budget: String,
    pub passed: bool,
}

impl Threshold {
    fn duration(name: &'static str, measured: Duration, budget: Duration) -> Self {
        Self {
            name,
            measured: format!("{:.1} ms", measured.as_secs_f64() * 1000.0),
            budget: format!("{:.0} ms", budget.as_secs_f64() * 1000.0),
            passed: measured <= budget,
        }
    }

    fn fact(name: &'static str, measured: impl Into<String>, passed: bool) -> Self {
        Self {
            name,
            measured: measured.into(),
            budget: "must hold".into(),
            passed,
        }
    }
}

/// Everything the verdict is written from.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub v8_version: String,
    pub binary_bytes: u64,
    pub process_cold_init_ms: f64,
    pub cold_session_ms: Percentiles,
    pub warm_cell_ms: Percentiles,
    pub heap_after_workload: HeapReport,
    pub native_rss_bytes: u64,
    pub native_rss_delta_bytes: i64,
    pub interrupt: InterruptReport,
    pub limits: LimitsReport,
    pub shutdown_joined: bool,
    pub thresholds: Vec<Threshold>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.thresholds.iter().all(|threshold| threshold.passed)
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Percentiles {
    pub samples: usize,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct HeapReport {
    pub used_bytes: usize,
    pub total_bytes: usize,
    pub limit_bytes: usize,
    pub external_bytes: usize,
}

/// What the external stop handle actually did to a runaway loop.
#[derive(Debug, Clone, Serialize)]
pub struct InterruptReport {
    pub budget_ms: u64,
    pub observed_ms: f64,
    /// Ticks a 10 ms Tokio task managed while the isolate was spinning. A
    /// blocked runtime would show a count near zero.
    pub tokio_ticks_during_spin: u64,
    pub failure: String,
    /// The isolate took another cell after the interruption.
    pub session_survived: bool,
}

/// Proof that the two memory ceilings are told apart (US-005 AC3).
#[derive(Debug, Clone, Serialize)]
pub struct LimitsReport {
    pub heap_failure: String,
    pub native_failure: String,
    pub distinguished: bool,
    pub process_alive: bool,
}

fn percentiles(mut samples: Vec<Duration>) -> Percentiles {
    samples.sort_unstable();
    let millis = |value: Duration| value.as_secs_f64() * 1000.0;
    let count = samples.len();
    if count == 0 {
        return Percentiles {
            samples: 0,
            min: 0.0,
            p50: 0.0,
            p95: 0.0,
            max: 0.0,
        };
    }
    let at = |ratio: f64| {
        let rank = ((count as f64) * ratio).ceil() as usize;
        millis(samples[rank.clamp(1, count) - 1])
    };
    Percentiles {
        samples: count,
        min: millis(samples[0]),
        p50: at(0.50),
        p95: at(0.95),
        max: millis(samples[count - 1]),
    }
}

/// Resident set size of the process, in bytes, read from procfs.
pub fn resident_bytes() -> u64 {
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    let Some(pages) = statm.split_whitespace().nth(1) else {
        return 0;
    };
    let pages: u64 = pages.parse().unwrap_or(0);
    // `sysconf(_SC_PAGESIZE)` is 4096 on the Linux x86_64 target of this version.
    pages.saturating_mul(4096)
}

/// Times an interruption end to end and checks the Tokio runtime kept running.
pub fn measure_interrupt(host: &IsolateHost) -> InterruptReport {
    let budget = Duration::from_millis(200);
    let ticks = Arc::new(AtomicU64::new(0));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build();
    let ticker = runtime.as_ref().ok().map(|runtime| {
        let ticks = Arc::clone(&ticks);
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                ticks.fetch_add(1, Ordering::Relaxed);
            }
        })
    });

    let started = Instant::now();
    let outcome = host.eval_with_budget("while (true) {}", budget);
    let observed = started.elapsed();
    if let Some(ticker) = ticker {
        ticker.abort();
    }
    let survived = host.eval("1 + 1").as_deref() == Ok("2");
    InterruptReport {
        budget_ms: budget.as_millis() as u64,
        observed_ms: observed.as_secs_f64() * 1000.0,
        tokio_ticks_during_spin: ticks.load(Ordering::Relaxed),
        failure: match outcome {
            Ok(value) => format!("no interruption, script returned {value}"),
            Err(failure) => failure.to_string(),
        },
        session_survived: survived,
    }
}

/// Breaches the two memory ceilings in turn and checks they are told apart.
pub fn measure_limits() -> LimitsReport {
    let heap_host = IsolateHost::start(IsolateLimits {
        heap_bytes: 16 * 1024 * 1024,
        native_bytes: 256 * 1024 * 1024,
        cpu_budget: Duration::from_secs(20),
    });
    let heap_failure = match heap_host {
        Ok(host) => {
            // Retained strings, so no garbage collection can reclaim them.
            let outcome = host.eval(
                "const kept = []; for (;;) { kept.push('x'.repeat(1 << 16)); } 'unreachable'",
            );
            let text = failure_text(outcome);
            drop(host);
            text
        }
        Err(detail) => format!("host start failed: {detail}"),
    };

    let native_host = IsolateHost::start(IsolateLimits {
        heap_bytes: 256 * 1024 * 1024,
        native_bytes: 8 * 1024 * 1024,
        cpu_budget: Duration::from_secs(20),
    });
    let native_failure = match native_host {
        Ok(host) => {
            // ArrayBuffers live in native memory, not in the V8 heap: the
            // budgeted allocator is the only thing that can refuse them.
            let outcome = host.eval(
                "const kept = []; for (;;) { kept.push(new ArrayBuffer(1 << 20)); } 'unreachable'",
            );
            let text = failure_text(outcome);
            drop(host);
            text
        }
        Err(detail) => format!("host start failed: {detail}"),
    };

    LimitsReport {
        distinguished: heap_failure.starts_with("v8 heap limit")
            && native_failure.starts_with("native memory budget"),
        heap_failure,
        native_failure,
        // Reaching this line at all is the proof: a `FatalProcessOutOfMemory`
        // would have aborted the process before it.
        process_alive: true,
    }
}

fn failure_text(outcome: Result<String, CellFailure>) -> String {
    match outcome {
        Ok(value) => format!("no failure, script returned {value}"),
        Err(failure) => failure.to_string(),
    }
}

/// Runs every measurement of US-005 and returns the report the verdict quotes.
pub fn run(cold_sessions: usize, warm_cells: usize) -> Result<Report, String> {
    let rss_before = resident_bytes();
    let cold_init = host::initialize(host::JitMode::Enabled)?;

    let mut cold = Vec::with_capacity(cold_sessions);
    for _ in 0..cold_sessions {
        let started = Instant::now();
        let host = IsolateHost::start(IsolateLimits::default())?;
        host.eval("1")
            .map_err(|failure| format!("cold session probe failed: {failure}"))?;
        cold.push(started.elapsed());
        drop(host);
    }

    let host = IsolateHost::start(IsolateLimits::default())?;
    let mut warm = Vec::with_capacity(warm_cells);
    for index in 0..warm_cells {
        let source = format!("(function () {{ return {index} * 2; }})()");
        let started = Instant::now();
        host.eval(&source)
            .map_err(|failure| format!("warm cell failed: {failure}"))?;
        warm.push(started.elapsed());
    }

    host.eval(
        "const rows = []; for (let i = 0; i < 200000; i++) { rows.push({ i }); } rows.length",
    )
    .map_err(|failure| format!("heap workload failed: {failure}"))?;
    let heap = host.heap().unwrap_or(HeapSnapshot::default());
    let interrupt = measure_interrupt(&host);
    let shutdown_joined = host.shutdown(Duration::from_secs(2));

    let limits = measure_limits();
    let rss_after = resident_bytes();
    let binary_bytes = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
        .unwrap_or(0);

    let cold = percentiles(cold);
    let warm = percentiles(warm);
    let thresholds = vec![
        Threshold::duration(
            "interruption d'une boucle infinie",
            Duration::from_secs_f64(interrupt.observed_ms / 1000.0),
            MAX_INTERRUPT,
        ),
        Threshold::fact(
            "runtime Tokio non bloqué pendant le spin",
            format!("{} ticks de 10 ms", interrupt.tokio_ticks_during_spin),
            interrupt.tokio_ticks_during_spin > 0,
        ),
        Threshold::fact(
            "session utilisable après interruption",
            interrupt.session_survived.to_string(),
            interrupt.session_survived,
        ),
        Threshold::duration(
            "démarrage de cellule chaude (P95)",
            Duration::from_secs_f64(warm.p95 / 1000.0),
            MAX_WARM_CELL_P95,
        ),
        Threshold::duration(
            "session froide prête (P95)",
            Duration::from_secs_f64(cold.p95 / 1000.0),
            MAX_COLD_SESSION_P95,
        ),
        Threshold::fact(
            "heap V8 et mémoire native distingués",
            format!("{} | {}", limits.heap_failure, limits.native_failure),
            limits.distinguished,
        ),
        Threshold::fact(
            "aucun processus tué silencieusement",
            limits.process_alive.to_string(),
            limits.process_alive,
        ),
        Threshold::fact(
            "worker joint au shutdown",
            shutdown_joined.to_string(),
            shutdown_joined,
        ),
    ];

    Ok(Report {
        v8_version: v8::VERSION_STRING.to_string(),
        binary_bytes,
        process_cold_init_ms: cold_init.as_secs_f64() * 1000.0,
        cold_session_ms: cold,
        warm_cell_ms: warm,
        heap_after_workload: HeapReport {
            used_bytes: heap.used_bytes,
            total_bytes: heap.total_bytes,
            limit_bytes: heap.limit_bytes,
            external_bytes: heap.external_bytes,
        },
        native_rss_bytes: rss_after,
        native_rss_delta_bytes: rss_after as i64 - rss_before as i64,
        interrupt,
        limits,
        shutdown_joined,
        thresholds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One process, one V8 initialization: the whole suite shares a single
    /// measurement run, since `run` is what the verdict is written from.
    fn report() -> &'static Report {
        static REPORT: std::sync::OnceLock<Report> = std::sync::OnceLock::new();
        REPORT.get_or_init(|| run(20, 200).expect("measurement run"))
    }

    #[test]
    fn infinite_loop_is_interrupted_under_one_second() {
        let report = report();
        assert!(
            report.interrupt.observed_ms < MAX_INTERRUPT.as_secs_f64() * 1000.0,
            "interruption took {} ms",
            report.interrupt.observed_ms
        );
        assert!(
            report.interrupt.failure.starts_with("cpu budget exceeded"),
            "unexpected failure: {}",
            report.interrupt.failure
        );
    }

    #[test]
    fn tokio_keeps_running_while_the_isolate_spins() {
        assert!(report().interrupt.tokio_ticks_during_spin > 0);
    }

    #[test]
    fn session_survives_its_own_interruption() {
        assert!(report().interrupt.session_survived);
    }

    #[test]
    fn heap_and_native_ceilings_are_distinguished() {
        let limits = &report().limits;
        assert!(
            limits.distinguished,
            "heap: {} / native: {}",
            limits.heap_failure, limits.native_failure
        );
    }

    #[test]
    fn shutdown_joins_the_worker() {
        assert!(report().shutdown_joined);
    }

    #[test]
    fn measured_latencies_stay_inside_the_prd_budget() {
        let report = report();
        assert!(
            report.warm_cell_ms.p95 <= MAX_WARM_CELL_P95.as_secs_f64() * 1000.0,
            "warm p95 = {} ms",
            report.warm_cell_ms.p95
        );
        assert!(
            report.cold_session_ms.p95 <= MAX_COLD_SESSION_P95.as_secs_f64() * 1000.0,
            "cold p95 = {} ms",
            report.cold_session_ms.p95
        );
    }

    #[test]
    fn a_second_jit_mode_is_refused_instead_of_silently_ignored() {
        // `report()` has already initialized the process with JIT enabled.
        let _ = report();
        let error = host::initialize(host::JitMode::Jitless)
            .expect_err("switching JIT mode after initialization must fail");
        assert!(error.contains("already initialized"), "{error}");
    }

    #[test]
    fn a_javascript_exception_is_not_reported_as_a_quota_breach() {
        let _ = report();
        let host = IsolateHost::start(IsolateLimits::default()).unwrap();
        let failure = host.eval("throw new Error('boom')").unwrap_err();
        assert!(
            matches!(&failure, CellFailure::Js(message) if message.contains("boom")),
            "{failure:?}"
        );
        assert_eq!(host.eval("40 + 2"), Ok("42".to_string()));
    }

    /// Cross-cell state has to be an explicit store, never the global object:
    /// this is the property US-006 builds `store`/`load` on top of.
    #[test]
    fn a_cell_never_inherits_another_cells_globals() {
        let _ = report();
        let host = IsolateHost::start(IsolateLimits::default()).unwrap();
        host.eval("globalThis.leaked = 'first'").unwrap();
        assert_eq!(
            host.eval("typeof globalThis.leaked"),
            Ok("undefined".to_string())
        );
        let other = IsolateHost::start(IsolateLimits::default()).unwrap();
        assert_eq!(
            other.eval("typeof globalThis.leaked"),
            Ok("undefined".to_string())
        );
    }
}
