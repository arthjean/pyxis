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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agent_core::RunConfig;
use agent_core::clock::Clock;
use agent_runtime::id::SequentialIds;

use crate::jsonl::{CapturedSink, OutputFormat};
use crate::runtime::{CliStepSource, EngineDeps, SettingsCell, TurnSettings};

/// The one recorded turn EP-038 needs: a text answer with its usage, no tool
/// call. The four scenario directories belong to EP-040; this scenario is what
/// the definition of done of EP-038 names.
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
async fn transcript_of(scenario: &'static str, seed: u64) -> Run {
    let workspace = TempWorkspace::new(scenario);
    // US-123 AC5, first half: the workspace is outside any repository, so
    // `TurnDiffTracker` has nothing to report. Asserted here rather than
    // assumed, because the second half (no `turn_diff` line in the stream) only
    // means something once this is true.
    assert!(
        !workspace.path.join(".git").exists(),
        "the scenario workspace must not be a git repository"
    );
    let registry = Arc::new(agent_tools::Registry::builder(&workspace.path).build());
    let steps = CliStepSource::new(Arc::clone(&registry), Vec::new());
    let provider = Arc::new(replay::ScriptedProvider::new(
        scenario,
        [("turn_final.sse", FINAL_TURN)],
    ));
    let sink = CapturedSink::default();
    let skills = crate::skills::Catalog {
        skills: Vec::new(),
        issues: Vec::new(),
    };

    let outcome = crate::headless::run(crate::headless::HeadlessRun {
        prompt: "Lis note.txt".to_string(),
        // Ephemeral: no file is opened, so no absolute path can reach the
        // stream through a session locator (US-018 AC4).
        session_path: None,
        // Fixed rather than derived: in an ephemeral run the binary itself
        // decides this label, so the harness decides it too. Nothing about it
        // is observed from the environment, which is what US-123 AC6 asks for.
        session_id: format!("{scenario}.jsonl"),
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

    assert!(
        outcome.is_ok(),
        "the scenario must end its turn: {outcome:?}"
    );
    provider.assert_consumed();
    Run {
        bytes: sink.bytes(),
        bodies: provider.bodies(),
    }
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
fn epoch_millisecond_tokens(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else {
            if current.len() == 13 && current.starts_with('1') {
                found.push(current.clone());
            }
            current.clear();
        }
    }
    if current.len() == 13 && current.starts_with('1') {
        found.push(current);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    use agent_core::message::Message;
    use agent_core::provider::{CanonicalRequest, Provider};

    /// US-123 AC1/AC3, and the definition of done of EP-038: the function the
    /// binary really calls renders the same bytes twice.
    ///
    /// The two runs do NOT share their environment: each opens its own
    /// temporary directory and each starts at a different instant of the real
    /// clock. So the comparison proves more than repeatability, it proves that
    /// neither of those two varying things reached the stream.
    #[tokio::test(start_paused = true)]
    async fn two_runs_of_the_published_headless_path_render_the_same_bytes() {
        let first = transcript_of("twice", SEED).await;
        let second = transcript_of("twice", SEED).await;

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
        let run = transcript_of("scan", SEED).await;
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
        let run = transcript_of("no-diff", SEED).await;
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
        let run = transcript_of("no-creds", SEED).await;
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
        let provider = replay::ScriptedProvider::new("beyond", [("turn_final.sse", FINAL_TURN)]);
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
            [("turn_final.sse", FINAL_TURN), ("second.sse", FINAL_TURN)],
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
}
