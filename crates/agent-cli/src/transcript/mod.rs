//! The published headless path, driven in test, byte for byte (US-123).
//!
//! What this module proves is not that the events are well formed: the unit
//! tests of `jsonl.rs` already do that on a writer built by hand. It proves
//! that the function the binary really calls, [`crate::headless::run`], renders
//! the same bytes twice. The order of the terminal states, the presence of the
//! `turn_diff` and the closing `Hook(Stop)` are decided inside that function;
//! a reconstitution would only prove what the reconstitution decided.
//!
//! Three sources of variation are removed rather than tolerated, and each is
//! removed by a dependency the run is given instead of one it builds:
//! the identifiers by [`agent_runtime::id::SequentialIds`] (US-121), the epoch
//! by [`FrozenClock`], and the provider by [`replay::ScriptedProvider`]
//! (US-122). What is left varying between the two runs of a test, the
//! temporary directory and the wall clock, is exactly what must not reach the
//! transcript, so the byte comparison is also what proves it does not.

mod replay;
mod scenario;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agent_core::RunConfig;
use agent_core::clock::Clock;
use agent_runtime::id::SequentialIds;

use crate::jsonl::{CapturedSink, OutputFormat};
use crate::runtime::{CliStepSource, EngineDeps, SettingsCell, TurnSettings};

use scenario::{Approval, Ending, Scenario};

/// One recorded turn, compiled in, for the unit tests that exercise the
/// scripted provider WITHOUT running a scenario: they assert on the provider's
/// own failure modes, so they must not depend on a scenario directory being
/// shaped a particular way. The scenarios themselves read their streams from
/// disk (US-126) and share nothing with this constant.
const FINAL_TURN: &str = include_str!("../../tests/fixtures/turn_final.sse");

/// Epoch frozen on a value that is not the current one and never will be again.
///
/// `now_ms` returning a constant is what makes the `*_ms` fields of the stream
/// comparable between two runs. `sleep` still goes through `tokio::time`, so a
/// backoff is virtualized by `start_paused` rather than suppressed: a run that
/// would really wait still waits, in zero real time.
pub struct FrozenClock(pub u64);

#[async_trait::async_trait]
impl Clock for FrozenClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
    async fn sleep(&self, dur: Duration) {
        tokio::time::sleep(dur).await;
    }
}

/// A distinct epoch per scenario, far from any plausible wall clock so a value
/// that leaked from `SystemTime::now` cannot be mistaken for it.
const FROZEN_EPOCH_MS: u64 = 1_000_000_000_000;

/// Throwaway working directory, removed on `Drop`. Deliberately unique per
/// call: the two runs a determinism test compares get DIFFERENT directories,
/// so a path that reached the transcript would break the byte comparison
/// instead of hiding inside it.
struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pyxis-transcript-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("a temporary workspace is creatable");
        // `$TMPDIR` is a symbolic link on some distributions and the path
        // validation of `agent-tools` compares canonicalized paths.
        let path = std::fs::canonicalize(&path).expect("the temporary workspace canonicalizes");
        Self { path }
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn settings(workspace: &Path) -> Arc<SettingsCell> {
    SettingsCell::new(TurnSettings {
        model: "gpt-5".into(),
        reasoning_effort: None,
        tool_guidelines: Vec::new(),
        goal: None,
        run_config: RunConfig {
            max_retries: 1,
            backoff_base_ms: 0,
            ..RunConfig::default()
        },
        permission_mode: "ask".into(),
        sandbox: "enforced (workspace)".into(),
        workspace: workspace.to_path_buf(),
        web_search: false,
    })
}

/// What one run of the published path produced: the bytes it rendered, and the
/// request bodies it composed on the way. The second is what proves no
/// credential travels, since a JSON body is the only thing a run sends out.
struct Run {
    bytes: Vec<u8>,
    bodies: Vec<serde_json::Value>,
}

/// One complete run of the published headless path.
///
/// Everything the binary resolves before calling `run` is resolved here the
/// same way, and nothing else: no credential is read, no keyring is opened, no
/// terminal is required. The provider answers from a recorded stream and the
/// session is ephemeral, so the run touches the disk only through a temporary
/// directory it creates and removes itself.
async fn transcript_of(scenario: &Scenario, seed: u64) -> Run {
    let workspace = TempWorkspace::new(&scenario.name);
    // US-123 AC5, first half: the workspace is outside any repository, so
    // `TurnDiffTracker` has nothing to report. Asserted here rather than
    // assumed, because the second half (no `turn_diff` line in the stream) only
    // means something once this is true.
    assert!(
        !workspace.path.join(".git").exists(),
        "the scenario workspace must not be a git repository"
    );
    for (path, content) in &scenario.files {
        std::fs::write(workspace.path.join(path), content).expect("a seeded file is writable");
    }

    // The sandbox is declared, not omitted: `write` validates its target
    // against it BEFORE the permission decision, so a registry without a policy
    // would refuse the call at validation and the interruption scenario would
    // never reach the approver it exists to exercise. `settings()` already
    // announces `enforced (workspace)`, so this is what it announces.
    let mut builder = agent_tools::Registry::builder(&workspace.path)
        .sandbox(
            agent_core::sandbox::SandboxPolicy::workspace_write(
                &workspace.path,
                [],
                [] as [&str; 0],
            ),
            false,
        )
        .register(agent_tools::Read)
        .register(agent_tools::Write);
    if let Some(approval) = scenario.approval {
        builder = builder.approver(Arc::new(ScriptedApprover(approval)));
    }
    let registry = Arc::new(builder.build());
    let steps = CliStepSource::new(Arc::clone(&registry), Vec::new());
    let provider = Arc::new(replay::ScriptedProvider::new(
        scenario.name.clone(),
        scenario.script.clone(),
    ));
    let sink = CapturedSink::default();
    let skills = crate::skills::Catalog {
        skills: Vec::new(),
        issues: Vec::new(),
    };

    let outcome = crate::headless::run(crate::headless::HeadlessRun {
        prompt: scenario.prompt.clone(),
        // Ephemeral: no file is opened, so no absolute path can reach the
        // stream through a session locator (US-018 AC4).
        session_path: None,
        // Fixed rather than derived: in an ephemeral run the binary itself
        // decides this label, so the harness decides it too. Nothing about it
        // is observed from the environment, which is what US-123 AC6 asks for.
        session_id: format!("{}.jsonl", scenario.name),
        workspace: workspace.path.clone(),
        output_format: OutputFormat::Json,
        output: Box::new(sink.clone()),
        output_last_message: None,
        hooks: Arc::new(agent_tools::hooks::NoHooks),
        skills: &skills,
        registry: Arc::clone(&registry),
        engine: EngineDeps {
            provider: Arc::clone(&provider) as Arc<dyn agent_core::provider::Provider>,
            tokenizer: Arc::new(agent_tokenizer::HeuristicCounter),
            clock: Arc::new(FrozenClock(FROZEN_EPOCH_MS)),
            ids: Arc::new(SequentialIds::starting_at(seed)),
            tools: Arc::clone(&registry) as Arc<dyn agent_core::tools::ToolDispatch>,
            context_window: Default::default(),
        },
        settings: settings(&workspace.path),
        steps: Arc::clone(&steps),
        agents: None,
    })
    .await;

    // An ending is asserted in BOTH directions: a scenario that recorded an
    // interruption and started succeeding renders different bytes for a reason
    // the byte comparison alone would not name.
    match scenario.ending {
        Ending::Ok => assert!(
            outcome.is_ok(),
            "scenario `{}` must end its turn: {outcome:?}",
            scenario.name
        ),
        Ending::Err => assert!(
            outcome.is_err(),
            "scenario `{}` records a run that does not reach `end_turn`",
            scenario.name
        ),
    }
    provider.assert_consumed();
    Run {
        bytes: sink.bytes(),
        bodies: provider.bodies(),
    }
}

/// The answer a scenario declared, given to every permission request of the run.
///
/// One answer rather than a queue: a scenario asks at most one question, and a
/// queue would let a recorded run drift into asking a second one unnoticed.
struct ScriptedApprover(Approval);

#[async_trait::async_trait]
impl agent_tools::permission::Approver for ScriptedApprover {
    async fn approve(
        &self,
        _req: &agent_tools::permission::PermissionRequest,
    ) -> agent_tools::permission::ApprovalResponse {
        match self.0 {
            Approval::Allow => agent_tools::permission::ApprovalResponse::ALLOW_ONCE,
            Approval::Deny => agent_tools::permission::ApprovalResponse::DENY_ONCE,
            Approval::Abort => agent_tools::permission::ApprovalResponse::Abort,
        }
    }
}

/// The scenario a US-123 assertion runs on when it needs a run rather than a
/// particular shape of run: the bare turn, which is the smallest one.
fn bare_turn() -> Scenario {
    named_scenario("bare-turn")
}

/// One scenario by name, with the failure naming what the tree holds instead.
fn named_scenario(name: &str) -> Scenario {
    let mut found = scenario::discover().expect("the scenario tree is well formed");
    let available: Vec<String> = found.iter().map(|s| s.name.clone()).collect();
    let at = found
        .iter()
        .position(|s| s.name == name)
        .unwrap_or_else(|| {
            unreachable!("scenario `{name}` is missing, the tree holds {available:?}")
        });
    found.swap_remove(at)
}

/// The seed both runs of a comparison start from. Any value works; a fixed one
/// is what makes the identifiers of a transcript readable across a diff.
const SEED: u64 = 1;

/// Tokens that look like a filesystem path: a run of path characters opening on
/// `/`. The transcript of a scenario carries none, and that is the point: an
/// absolute path is the first thing a machine-readable stream leaks about the
/// machine that produced it.
fn absolute_path_tokens(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || "/._-~".contains(ch) {
            current.push(ch);
        } else {
            if current.starts_with('/') {
                found.push(current.clone());
            }
            current.clear();
        }
    }
    if current.starts_with('/') {
        found.push(current);
    }
    found
}

/// Integers of thirteen digits opening on `1`: every epoch in milliseconds
/// between 2001 and 2033. A transcript that carries one carries the moment it
/// was produced, which is the definition of a byte that varies per run.
///
/// The one the harness froze is excluded, and only that one: it has the shape
/// of a wall clock because it IS an epoch, but it is the same epoch on every
/// run, which is exactly what this scan looks for the absence of. An interrupted
/// turn publishes it (`started_at_ms`), so the exclusion is load-bearing rather
/// than defensive.
fn epoch_millisecond_tokens(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else {
            if is_wall_clock(&current) {
                found.push(current.clone());
            }
            current.clear();
        }
    }
    if is_wall_clock(&current) {
        found.push(current);
    }
    found
}

fn is_wall_clock(token: &str) -> bool {
    token.len() == 13 && token.starts_with('1') && token != FROZEN_EPOCH_MS.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use agent_core::message::Message;
    use agent_core::provider::{CanonicalRequest, Provider};

    /// One script entry, for the tests that build a provider by hand.
    fn entry(name: &str, sse: &str) -> (String, String) {
        (name.to_string(), sse.to_string())
    }

    /// US-123 AC1/AC3, and the definition of done of EP-038: the function the
    /// binary really calls renders the same bytes twice.
    ///
    /// The two runs do NOT share their environment: each opens its own
    /// temporary directory and each starts at a different instant of the real
    /// clock. So the comparison proves more than repeatability, it proves that
    /// neither of those two varying things reached the stream.
    #[tokio::test(start_paused = true)]
    async fn two_runs_of_the_published_headless_path_render_the_same_bytes() {
        let scenario = bare_turn();
        let first = transcript_of(&scenario, SEED).await;
        let second = transcript_of(&scenario, SEED).await;

        assert!(!first.bytes.is_empty(), "a run renders something");
        assert_eq!(
            String::from_utf8_lossy(&first.bytes),
            String::from_utf8_lossy(&second.bytes),
            "two runs of the same scenario must render the same transcript"
        );
        assert_eq!(
            first.bytes, second.bytes,
            "byte for byte, not line for line"
        );
    }

    /// US-123 AC4: the scan is an assertion, not a reading. What it looks for is
    /// what a transcript leaks when nobody looks: the path of the machine, the
    /// moment of the run, and anything else that moves from one run to the next.
    #[tokio::test(start_paused = true)]
    async fn the_transcript_carries_no_absolute_path_and_no_wall_clock() {
        let run = transcript_of(&bare_turn(), SEED).await;
        let text = String::from_utf8_lossy(&run.bytes).into_owned();

        let paths = absolute_path_tokens(&text);
        assert!(
            paths.is_empty(),
            "an absolute path reached the transcript: {paths:?}"
        );
        let epochs = epoch_millisecond_tokens(&text);
        assert!(
            epochs.is_empty(),
            "a wall-clock timestamp reached the transcript: {epochs:?}"
        );
        assert!(
            !text.contains(&std::env::temp_dir().to_string_lossy().to_string()),
            "the temporary root reached the transcript: {text}"
        );
        // Every identifier comes from the seeded generator, so it is a function
        // of the seed and of nothing else. A random one would break the byte
        // comparison above; naming the shape here says WHY it does not.
        for line in text.lines() {
            for prefix in ["thr_", "trn_", "evt_"] {
                if let Some(at) = line.find(prefix) {
                    let id = &line[at..at + prefix.len() + 32];
                    assert!(
                        id.ends_with(|ch: char| ch.is_ascii_hexdigit()),
                        "identifier {id} is not the seeded shape"
                    );
                }
            }
        }
    }

    /// US-123 AC5, second half: the workspace is outside git, so the aggregated
    /// diff has nothing to say and `headless::run` emits no `turn_diff`, as
    /// `docs/EVENT_SCHEMA.md` describes. Asserted rather than endured: the line
    /// carries file paths, so its silent appearance would be a leak.
    #[tokio::test(start_paused = true)]
    async fn a_workspace_outside_git_emits_no_turn_diff() {
        let run = transcript_of(&bare_turn(), SEED).await;
        let text = String::from_utf8_lossy(&run.bytes).into_owned();
        assert!(
            !text.contains("\"type\":\"turn_diff\""),
            "no turn_diff is expected outside a repository: {text}"
        );
    }

    /// US-122 AC5 and US-123 AC7: the harness reads no credential and opens no
    /// keyring. Checked where it would show: the composed request body is the
    /// only thing a run sends out, and the transcript is the only thing it
    /// writes.
    #[tokio::test(start_paused = true)]
    async fn the_harness_carries_no_credential_out_and_none_into_the_transcript() {
        let run = transcript_of(&bare_turn(), SEED).await;
        assert_eq!(run.bodies.len(), 1, "one model turn, one composed body");
        for body in &run.bodies {
            let rendered = serde_json::to_string(body).expect("a composed body serializes");
            assert!(
                !rendered.contains("Bearer") && !rendered.contains("access_token"),
                "no credential may travel in the request body: {rendered}"
            );
        }
        let text = String::from_utf8_lossy(&run.bytes).into_owned();
        assert!(
            !text.contains("Bearer") && !text.contains("access_token"),
            "no credential may reach the transcript: {text}"
        );
    }

    /// US-122 AC2: one request past the script is a named error, never an empty
    /// stream. An empty stream would decode as a turn that said nothing, and the
    /// scenario would go green one request beyond what it recorded.
    #[tokio::test]
    async fn a_request_beyond_the_script_names_the_scenario_and_the_rank() {
        let provider =
            replay::ScriptedProvider::new("beyond", [entry("turn_final.sse", FINAL_TURN)]);
        let request = || CanonicalRequest {
            model: "gpt-5".to_string(),
            messages: vec![Message::user("Réponds")],
            max_output_tokens: 4096,
            ..CanonicalRequest::default()
        };

        provider
            .stream(request())
            .await
            .map(|_| ())
            .expect("the first request is inside the script");
        let err = match provider.stream(request()).await {
            Ok(_) => unreachable!("a request beyond the script must not open a stream"),
            Err(err) => err.to_string(),
        };
        assert!(
            err.contains("beyond") && err.contains("`beyond`") && err.contains("#2"),
            "the error must name the scenario and the rank of the request: {err}"
        );
    }

    /// US-122 AC3/AC4: the teardown assertion is the difference between a
    /// scenario that finished and a scenario that stopped. Silent on a script
    /// played to the end, and naming what stayed in the queue otherwise.
    #[test]
    fn the_teardown_assertion_names_what_the_scenario_never_played() {
        let played = replay::ScriptedProvider::new("silent", std::iter::empty());
        played.assert_consumed();

        let half = replay::ScriptedProvider::new(
            "half",
            [
                entry("turn_final.sse", FINAL_TURN),
                entry("second.sse", FINAL_TURN),
            ],
        );
        let failure =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| half.assert_consumed()))
                .expect_err("a half played script must fail the teardown");
        let message = failure
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(
            message.contains("turn_final.sse") && message.contains("second.sse"),
            "the failure must name the remaining entries: {message}"
        );
    }

    /// US-124 AC1/AC2 and US-126: THE gate. Every scenario the tree holds is
    /// run through the published headless path and its bytes are compared to
    /// the transcript frozen beside it, raw against raw.
    ///
    /// The switch writes and returns, exactly like
    /// `crates/agent-app-server/tests/schemas.rs`: a regeneration is not a
    /// comparison that happens to pass, it is a comparison that did not
    /// happen, and conflating the two is how a gate certifies its own output.
    #[tokio::test(start_paused = true)]
    async fn every_scenario_renders_the_transcript_frozen_beside_it() {
        let update = std::env::var_os(scenario::UPDATE_VARIABLE).is_some();
        let scenarios = scenario::discover().expect("the scenario tree is well formed");
        assert!(!scenarios.is_empty(), "the scenario tree is not empty");

        let mut drifts = Vec::new();
        for scenario in &scenarios {
            let run = transcript_of(scenario, SEED).await;
            if update {
                std::fs::write(&scenario.expected, &run.bytes)
                    .expect("the frozen transcript is writable");
                continue;
            }
            if let Err(drift) =
                scenario::transcript_verdict(&scenario.name, &scenario.expected, &run.bytes)
            {
                drifts.push(drift);
            }
            // US-123 AC4, now on every scenario rather than on one: the two
            // things a transcript leaks about the machine that produced it are
            // where it ran and when.
            let text = String::from_utf8_lossy(&run.bytes).into_owned();
            let paths = absolute_path_tokens(&text);
            assert!(
                paths.is_empty(),
                "scenario `{}`: an absolute path reached the transcript: {paths:?}",
                scenario.name
            );
            let epochs = epoch_millisecond_tokens(&text);
            assert!(
                epochs.is_empty(),
                "scenario `{}`: a wall-clock timestamp reached the transcript: {epochs:?}",
                scenario.name
            );
        }
        assert!(drifts.is_empty(), "{}", drifts.join("\n"));
    }

    /// US-128 AC4: the order rule applied to what a run PRODUCES, kept apart
    /// from the byte comparison on purpose. Inverting the two terminals in
    /// `headless.rs` fails this test AND the gate above, and it takes two
    /// failures to read the change as a contract violation: one alone reads as
    /// a stale file and gets regenerated away.
    #[tokio::test(start_paused = true)]
    async fn every_scenario_produces_the_terminal_order_the_document_publishes() {
        let scenarios = scenario::discover().expect("the scenario tree is well formed");
        let mut drifts = Vec::new();
        for scenario in &scenarios {
            let run = transcript_of(scenario, SEED).await;
            if let Err(drift) = scenario::terminal_order_verdict(&scenario.name, &run.bytes) {
                drifts.push(drift);
            }
        }
        assert!(drifts.is_empty(), "{}", drifts.join("\n"));
    }

    /// US-128 AC1/AC2/AC5: the order the document publishes, read off the
    /// FROZEN files. It holds without rerunning the harness, and it is the half
    /// the byte comparison cannot provide: a transcript regenerated after an
    /// inversion is byte-identical to itself and says nothing.
    #[test]
    fn the_frozen_transcripts_close_on_the_summary_and_at_most_one_stop_hook() {
        let scenarios = scenario::discover().expect("the scenario tree is well formed");
        for scenario in &scenarios {
            let frozen = std::fs::read(&scenario.expected).unwrap_or_else(|_| {
                unreachable!("the transcript of `{}` is frozen", scenario.name)
            });
            scenario::terminal_order_verdict(&scenario.name, &frozen)
                .unwrap_or_else(|verdict| unreachable!("{verdict}"));
        }
    }

    /// US-128 AC3: the absence is observed, not skipped. Every scenario runs in
    /// a temporary workspace outside any repository, so `turn_diff` is never
    /// emitted; a test that only checked the order would be satisfied by that
    /// silence and would never notice the line reappearing after the summary.
    #[test]
    fn no_frozen_transcript_carries_a_turn_diff_because_every_scenario_runs_outside_git() {
        let scenarios = scenario::discover().expect("the scenario tree is well formed");
        for scenario in &scenarios {
            let frozen = std::fs::read(&scenario.expected).unwrap_or_else(|_| {
                unreachable!("the transcript of `{}` is frozen", scenario.name)
            });
            assert!(
                !scenario::carries_turn_diff(&frozen),
                "scenario `{}` froze a turn_diff, which no workspace outside git can produce",
                scenario.name
            );
        }
    }

    /// US-128 AC4, on the verdict itself: swapping the two terminals is refused
    /// rather than tolerated as "a hook came late".
    #[test]
    fn a_summary_written_after_the_stop_hook_is_refused() {
        let inverted = concat!(
            r#"{"type":"end_turn"}"#,
            "
",
            r#"{"type":"hook","data":{"event":"Stop","status":"completed"}}"#,
            "
",
            r#"{"type":"run_summary","data":{"end":"end_turn"}}"#,
            "
"
        );
        let verdict = scenario::terminal_order_verdict("inverted", inverted.as_bytes())
            .expect_err("a run that ends on end_turn without its hook cannot pass");
        assert!(verdict.contains("end_turn"), "{verdict}");
        assert!(verdict.contains("hook"), "{verdict}");
    }

    /// The three other shapes the rule refuses, each for its own reason: a line
    /// that is not the `Stop` hook after the summary, a hook on a run the agent
    /// did not end itself, and a diff published after the summary.
    #[test]
    fn nothing_but_the_stop_hook_may_follow_the_summary() {
        let trailing_text = concat!(
            r#"{"type":"run_summary","data":{"end":"end_turn"}}"#,
            "
",
            r#"{"type":"hook","data":{"event":"Stop","status":"completed"}}"#,
            "
",
            r#"{"type":"text","data":"après coup"}"#,
            "
"
        );
        let verdict = scenario::terminal_order_verdict("trailing", trailing_text.as_bytes())
            .expect_err("a text line after the summary cannot pass");
        assert!(verdict.contains("run_summary"), "{verdict}");

        let hook_on_failure = concat!(
            r#"{"type":"run_summary","data":{"end":"error"}}"#,
            "
",
            r#"{"type":"hook","data":{"event":"Stop","status":"completed"}}"#,
            "
"
        );
        let verdict = scenario::terminal_order_verdict("failed", hook_on_failure.as_bytes())
            .expect_err("a Stop hook on a failed run cannot pass");
        assert!(verdict.contains("error"), "{verdict}");

        let late_diff = concat!(
            r#"{"type":"run_summary","data":{"end":"interrupted"}}"#,
            "
",
            r#"{"type":"turn_diff","data":{"files":[]}}"#,
            "
"
        );
        let verdict = scenario::terminal_order_verdict("late-diff", late_diff.as_bytes())
            .expect_err("a diff after the summary cannot pass");
        assert!(verdict.contains("turn_diff"), "{verdict}");
    }

    /// A run with no summary, or with two, is a malformed transcript rather than
    /// an ordering question, and the verdict says which of the two it saw.
    #[test]
    fn a_transcript_without_exactly_one_summary_names_how_many_it_holds() {
        let none = scenario::terminal_order_verdict("none", br#"{"type":"end_turn"}"# as &[u8])
            .expect_err("a transcript with no summary cannot pass");
        assert!(none.contains("0 `run_summary`"), "{none}");

        let twice = concat!(
            r#"{"type":"run_summary","data":{"end":"end_turn"}}"#,
            "
",
            r#"{"type":"run_summary","data":{"end":"end_turn"}}"#,
            "
"
        );
        let verdict = scenario::terminal_order_verdict("twice", twice.as_bytes())
            .expect_err("two summaries cannot pass");
        assert!(verdict.contains("2 `run_summary`"), "{verdict}");
    }

    /// US-126 AC1: the four behaviors the epic names are covered. Asserted on
    /// the discovered names rather than on a count, because "four directories"
    /// is satisfied by four copies of the same turn.
    #[test]
    fn the_tree_covers_the_bare_turn_the_tool_the_interruption_and_the_error() {
        let found: Vec<String> = scenario::discover()
            .expect("the scenario tree is well formed")
            .into_iter()
            .map(|scenario| scenario.name)
            .collect();
        for expected in ["bare-turn", "tool-call", "interruption", "stream-error"] {
            assert!(
                found.iter().any(|name| name == expected),
                "scenario `{expected}` is missing, the tree holds {found:?}"
            );
        }
    }

    /// US-124 AC4: no `trim` anywhere means the last byte is part of the
    /// contract, and a checkout that rewrote the line endings would break every
    /// comparison at once. Read from the files themselves, so what is asserted
    /// is what git handed over.
    #[test]
    fn the_frozen_transcripts_end_on_a_newline_and_carry_no_carriage_return() {
        let scenarios = scenario::discover().expect("the scenario tree is well formed");
        for scenario in &scenarios {
            let frozen = std::fs::read(&scenario.expected).unwrap_or_else(|_| {
                unreachable!("the transcript of `{}` is frozen", scenario.name)
            });
            scenario::line_ending_verdict(&scenario.name, &scenario.expected, &frozen)
                .unwrap_or_else(|verdict| unreachable!("{verdict}"));
        }
    }

    /// The NFR, as an assertion: a transcript is a proof a human reads in a
    /// diff, so a scenario that grew past what a human reads stopped being one.
    #[test]
    fn the_frozen_tree_stays_inside_its_budget() {
        let scenarios = scenario::discover().expect("the scenario tree is well formed");
        let mut total = 0u64;
        for scenario in &scenarios {
            let mut size = 0u64;
            for (_, sse) in &scenario.script {
                size += sse.len() as u64;
            }
            size += std::fs::metadata(&scenario.expected)
                .map(|meta| meta.len())
                .unwrap_or_default();
            assert!(
                size <= scenario::SCENARIO_BUDGET_BYTES,
                "scenario `{}` weighs {size} bytes, over the {} it is allowed",
                scenario.name,
                scenario::SCENARIO_BUDGET_BYTES
            );
            total += size;
        }
        assert!(
            total <= scenario::TREE_BUDGET_BYTES,
            "the frozen tree weighs {total} bytes, over the {} it is allowed",
            scenario::TREE_BUDGET_BYTES
        );
    }

    /// US-124 AC3: an absent transcript is a verdict, not a file the gate
    /// creates for itself. Proved on a path that does not exist, and the
    /// absence of a side effect is proved with it: a gate that writes what it
    /// then compares always passes.
    #[test]
    fn an_absent_transcript_names_the_scenario_and_the_regeneration_command() {
        let missing = std::env::temp_dir().join("pyxis-transcript-never-frozen/expected.jsonl");
        let verdict = scenario::transcript_verdict("ghost", &missing, b"{}\n")
            .expect_err("an absent transcript cannot pass");
        assert!(
            verdict.contains("`ghost`") && verdict.contains(scenario::UPDATE_COMMAND),
            "the verdict must name the scenario and the command: {verdict}"
        );
        assert!(
            !missing.exists(),
            "comparing must not create the file it compares against"
        );
    }

    /// US-124 AC2: one byte is enough, and the verdict says which path is stale
    /// and what refreshes it.
    #[test]
    fn one_byte_of_difference_names_the_path_and_the_regeneration_command() {
        let scenario = bare_turn();
        let frozen = std::fs::read(&scenario.expected)
            .unwrap_or_else(|_| unreachable!("the transcript of `{}` is frozen", scenario.name));
        scenario::transcript_verdict(&scenario.name, &scenario.expected, &frozen)
            .unwrap_or_else(|verdict| unreachable!("{verdict}"));

        let mut drifted = frozen.clone();
        let last = drifted.len() - 2;
        drifted[last] ^= 0x01;
        let verdict = scenario::transcript_verdict(&scenario.name, &scenario.expected, &drifted)
            .expect_err("one byte of difference cannot pass");
        assert!(
            verdict.contains("expected.jsonl")
                && verdict.contains("stale")
                && verdict.contains(scenario::UPDATE_COMMAND),
            "the verdict must name the path and the command: {verdict}"
        );
    }

    /// US-124 AC4, on the verdict rather than on the tree: what the assertion
    /// above would say if a checkout ever rewrote the line endings.
    #[test]
    fn a_carriage_return_in_a_transcript_points_at_the_gitattributes_entry() {
        let path = Path::new("tests/transcripts/bare-turn/expected.jsonl");
        let verdict = scenario::line_ending_verdict("bare-turn", path, b"{}\r\n")
            .expect_err("a carriage return cannot pass");
        assert!(
            verdict.contains(".gitattributes"),
            "the verdict must point at the pin: {verdict}"
        );
        let verdict = scenario::line_ending_verdict("bare-turn", path, b"{}")
            .expect_err("a transcript without its final newline cannot pass");
        assert!(
            verdict.contains("newline"),
            "the verdict must name the missing newline: {verdict}"
        );
    }
}
