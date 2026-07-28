//! `s6-code-mode-v8 report` prints the US-005 measurement report. The
//! deterministic proofs live in `cargo test -p s6-code-mode-v8`; this binary
//! exists so the numbers quoted by the verdict can be reproduced by hand.

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "report".into());
    let json = std::env::args().any(|argument| argument == "--json");
    if mode != "report" {
        eprintln!("usage: s6-code-mode-v8 [report] [--json]");
        std::process::exit(2);
    }

    let report = match spike_v8::run(100, 1000) {
        Ok(report) => report,
        Err(detail) => {
            eprintln!("mesure impossible: {detail}");
            std::process::exit(1);
        }
    };

    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("sérialisation impossible: {error}");
                std::process::exit(1);
            }
        }
    } else {
        print_report(&report);
    }
    std::process::exit(if report.passed() { 0 } else { 1 });
}

fn print_report(report: &spike_v8::Report) {
    let mib = |bytes: f64| bytes / (1024.0 * 1024.0);
    println!("V8            : {}", report.v8_version);
    println!(
        "binaire       : {:.1} MiB (profil de build courant)",
        mib(report.binary_bytes as f64)
    );
    println!(
        "init process  : {:.1} ms (ICU + platform + V8::initialize, une fois)",
        report.process_cold_init_ms
    );
    println!(
        "session froide: p50 {:.1} ms / p95 {:.1} ms / max {:.1} ms sur {}",
        report.cold_session_ms.p50,
        report.cold_session_ms.p95,
        report.cold_session_ms.max,
        report.cold_session_ms.samples
    );
    println!(
        "cellule chaude: p50 {:.1} ms / p95 {:.1} ms / max {:.1} ms sur {}",
        report.warm_cell_ms.p50,
        report.warm_cell_ms.p95,
        report.warm_cell_ms.max,
        report.warm_cell_ms.samples
    );
    println!(
        "heap V8       : {:.1} MiB utilisés / {:.1} MiB alloués / plafond {:.1} MiB / externe {:.1} MiB",
        mib(report.heap_after_workload.used_bytes as f64),
        mib(report.heap_after_workload.total_bytes as f64),
        mib(report.heap_after_workload.limit_bytes as f64),
        mib(report.heap_after_workload.external_bytes as f64)
    );
    println!(
        "mémoire native: RSS {:.1} MiB (delta {:+.1} MiB)",
        mib(report.native_rss_bytes as f64),
        mib(report.native_rss_delta_bytes as f64)
    );
    println!(
        "interruption  : {:.1} ms pour un budget de {} ms, {} ticks Tokio pendant le spin",
        report.interrupt.observed_ms,
        report.interrupt.budget_ms,
        report.interrupt.tokio_ticks_during_spin
    );
    println!("  cause       : {}", report.interrupt.failure);
    println!("limites       :");
    println!("  heap        : {}", report.limits.heap_failure);
    println!("  native      : {}", report.limits.native_failure);
    println!();
    for threshold in &report.thresholds {
        println!(
            "[{}] {} : {} (budget {})",
            if threshold.passed { "OK" } else { "KO" },
            threshold.name,
            threshold.measured,
            threshold.budget
        );
    }
    println!();
    println!(
        "VERDICT : {}",
        if report.passed() {
            "in-process tenable"
        } else {
            "seuil manqué, US-006 bloquée avec recommandation process-owned"
        }
    );
}
