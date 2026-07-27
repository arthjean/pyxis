//! End-to-end integration harness (US-007, `tasks/prd-harness-parity.md`).
//!
//! What this harness checks, and what the unit tests of each crate cannot
//! see: the **wiring**. A full turn here goes through the core
//! (`agent-core`), the real SSE decoder and request builder
//! (`agent-provider`), the real tool registry (`agent-tools`) and the real
//! JSONL persistence (`agent-session`), on a temporary directory.
//!
//! The provider is simulated at the **transport** level only: it replays a
//! recorded SSE stream (`tests/fixtures/*.sse`) through the real
//! `CodexEventMapper`, and composes the outgoing request with the real
//! `build_responses_body`. Not a byte goes on the network, no keyring
//! is opened, no terminal is required (AC3).
//!
//! The Landlock sandbox is NOT applied here: `restrict_self` is irreversible and
//! applies to the whole process, so applying it would confine the test
//! harness itself. The workspace boundary stays checked by the path validation
//! of `agent-tools`, which is well within the traversed perimeter.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::clock::SystemClock;
use agent_core::message::{ContentBlock, Message, Role};
use agent_core::provider::{
    CanonicalRequest, CanonicalResponse, Capabilities, ErrorClass, Provider, ProviderError,
    ProviderKind, StreamEvent,
};
use agent_core::{AgentContext, CancelToken, Deps, HeadlessEnd, RunConfig, run_headless};
use agent_provider::CodexEventMapper;
use agent_provider::chatgpt_request::{ResponsesBodyOptions, build_responses_body};
use agent_session::JsonlSession;
use agent_tokenizer::HeuristicCounter;
use agent_tools::error::{ToolError, ValidationError};
use agent_tools::permission::{PermCtx, PermissionDecision};
use agent_tools::tool::{ToolCtx, ToolOutput};
use agent_tools::{Read, Registry, Tool};
use futures_util::stream::BoxStream;

const TOOL_CALL_TURN: &str = include_str!("fixtures/turn_tool_call.sse");
const FINAL_TURN: &str = include_str!("fixtures/turn_final.sse");
const BLOCKING_TURN: &str = include_str!("fixtures/turn_blocking_tool.sse");
const MALFORMED_TURN: &str = include_str!("fixtures/turn_malformed.sse");

// ───────────────────────── Temporary directory ─────────────────────────

/// Throwaway working directory. Written in `$TMPDIR`, removed on `Drop`;
/// no new dependency for fifteen lines.
struct TempWorkspace {
    path: PathBuf,
}

impl TempWorkspace {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("pyxis-e2e-{tag}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        // The workspace is anchored on the canonical path: `$TMPDIR` is a symbolic
        // link on some distributions, and the path validation
        // of `agent-tools` compares canonicalized paths.
        let path = std::fs::canonicalize(&path).unwrap();
        Self { path }
    }

    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.path.join(name), content).unwrap();
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ───────────────────────── SSE replay provider ─────────────────────────

/// Extracts the `data:` payloads of a recorded SSE stream, in order.
fn sse_payloads(raw: &str) -> Vec<String> {
    raw.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|payload| payload.trim().to_string())
        .filter(|payload| !payload.is_empty())
        .collect()
}

/// Simulated provider: one recorded SSE stream per turn, decoded by the real
/// mapper. The composed requests are kept for inspection.
struct ReplayProvider {
    turns: Mutex<VecDeque<&'static str>>,
    bodies: Mutex<Vec<serde_json::Value>>,
    capabilities: Capabilities,
}

impl ReplayProvider {
    fn new(turns: impl IntoIterator<Item = &'static str>) -> Self {
        Self {
            turns: Mutex::new(turns.into_iter().collect()),
            bodies: Mutex::new(Vec::new()),
            capabilities: Capabilities {
                tools: true,
                max_context: 128_000,
                ..Capabilities::default()
            },
        }
    }

    fn bodies(&self) -> Vec<serde_json::Value> {
        self.bodies.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Provider for ReplayProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiChatGpt
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn stream(
        &self,
        req: CanonicalRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        // REAL composition path: it is the one carrying the guardrail against
        // orphan tool calls (US-003). A broken transcript would fail here,
        // not in a mock.
        let body = build_responses_body(&req, ResponsesBodyOptions::default());
        self.bodies.lock().unwrap().push(body);

        let Some(raw) = self.turns.lock().unwrap().pop_front() else {
            return Err(ProviderError::Transport(
                "replay provider: no scripted turn left".to_string(),
            ));
        };

        // REAL decoding of the recorded stream, including the "a stream without
        // a terminal event is a contract error" rule of the ChatGPT provider.
        let mut mapper = CodexEventMapper::new();
        let mut events: Vec<Result<StreamEvent, ProviderError>> = Vec::new();
        let mut saw_terminal = false;
        for payload in sse_payloads(raw) {
            match mapper.ingest(&payload) {
                Ok(decoded) => {
                    for event in decoded {
                        saw_terminal |= matches!(event, StreamEvent::Done { .. });
                        events.push(Ok(event));
                    }
                }
                Err(e) => {
                    events.push(Err(e));
                    saw_terminal = true;
                    break;
                }
            }
        }
        if !saw_terminal {
            events.push(Err(ProviderError::Stream("missing terminal event".into())));
        }
        Ok(Box::pin(futures_util::stream::iter(events)))
    }

    async fn complete(&self, _req: CanonicalRequest) -> Result<CanonicalResponse, ProviderError> {
        // Compaction is never reached by these scenarios (short context).
        Err(ProviderError::Transport(
            "replay provider: complete() is out of scope".to_string(),
        ))
    }

    fn classify_error(&self, _err: &ProviderError) -> ErrorClass {
        // A recorded stream does not repair itself by retrying: the failure must be
        // terminal and deterministic, otherwise a contract test would loop forever.
        ErrorClass::InvalidRequest
    }
}

// ─────────────────────────── Blocking tool ───────────────────────────

/// Tool that signals its start then never finishes: gives the test a
/// deterministic window to interrupt DURING a dispatch, without depending on an
/// arbitrary `sleep`.
///
/// The input stays a raw `Value`: `agent-cli` does not depend on `serde`
/// directly, and this test has no reason to add one.
struct BlockingTool {
    started: tokio::sync::mpsc::UnboundedSender<()>,
}

#[async_trait::async_trait]
impl Tool for BlockingTool {
    type Input = serde_json::Value;

    fn name(&self) -> &str {
        "blocking"
    }

    fn description(&self) -> String {
        "Test-only tool that never returns".to_string()
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "label": { "type": "string" } },
            "required": ["label"],
            "additionalProperties": false,
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_sensitive(&self) -> bool {
        false
    }

    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }

    fn validate_input(&self, _input: &Self::Input, _ctx: &ToolCtx) -> Result<(), ValidationError> {
        Ok(())
    }

    async fn call(&self, _input: Self::Input, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let _ = self.started.send(());
        std::future::pending::<()>().await;
        unreachable!("le dispatch est abandonné avant que cet outil ne rende la main")
    }
}

// ─────────────────────────── Harness wiring ───────────────────────────

struct Harness {
    provider: Arc<ReplayProvider>,
    deps: Deps,
    session_path: PathBuf,
    /// Specs exposed to the model, derived from the REAL registry: the test announces
    /// exactly the tools that the dispatch will know how to run.
    tool_specs: Vec<agent_core::provider::ToolSpec>,
}

fn harness(
    workspace: &TempWorkspace,
    turns: impl IntoIterator<Item = &'static str>,
    extra_tool: Option<BlockingTool>,
) -> Harness {
    harness_with_hooks(workspace, turns, extra_tool, None)
}

/// Same wiring, with a hook engine plugged into the registry (US-017).
fn harness_with_hooks(
    workspace: &TempWorkspace,
    turns: impl IntoIterator<Item = &'static str>,
    extra_tool: Option<BlockingTool>,
    hooks: Option<Arc<dyn agent_tools::hooks::Hooks>>,
) -> Harness {
    let sessions_dir = workspace.path.join(".pyxis").join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let session_path = sessions_dir.join("e2e.jsonl");
    let session = JsonlSession::create_at(&session_path).unwrap();

    let mut builder = Registry::builder(&workspace.path).register(Read);
    if let Some(tool) = extra_tool {
        builder = builder.register(tool);
    }
    if let Some(hooks) = hooks {
        builder = builder.hooks(hooks);
    }
    let registry = builder.build();
    let tool_specs = registry.tool_specs();

    let provider = Arc::new(ReplayProvider::new(turns));
    Harness {
        deps: Deps {
            provider: Arc::clone(&provider) as Arc<dyn Provider>,
            session: Arc::new(session),
            tokenizer: Arc::new(HeuristicCounter),
            clock: Arc::new(SystemClock),
            tools: Arc::new(registry),
            cancel: CancelToken::new(),
        },
        provider,
        session_path,
        tool_specs,
    }
}

fn context(prompt: &str, tools: Vec<agent_core::provider::ToolSpec>) -> AgentContext {
    AgentContext {
        model: "gpt-5".to_string(),
        reasoning_effort: None,
        system: Some("Tu es Pyxis en test d'intégration.".to_string()),
        messages: vec![Message::user(prompt)],
        tools,
        config: RunConfig::default(),
        context_messages: Vec::new(),
        ephemeral_messages: Vec::new(),
        step_source: None,
        inputs: None,
    }
}

/// Reads back the persisted session and returns the replayed messages, as
/// `pyxis --resume` would.
fn resumed_messages(path: &std::path::Path) -> Vec<Message> {
    agent_session::resume_file(path).unwrap().messages
}

/// Tool calls without a matching result in a transcript.
fn orphan_tool_calls(messages: &[Message]) -> Vec<String> {
    agent_core::message::unanswered_tool_calls(messages)
}

// ───────────────────────────────── Tests ─────────────────────────────────

/// AC1: a full turn (real tool call then final reply) unfolds
/// end to end on a temporary directory, without the network.
#[tokio::test]
async fn full_turn_with_tool_call_runs_without_network() {
    let workspace = TempWorkspace::new("full-turn");
    workspace.write("note.txt", "la phrase attendue\n");

    let h = harness(&workspace, [TOOL_CALL_TURN, FINAL_TURN], None);
    let result = run_headless(
        context("Lis note.txt", h.tool_specs.clone()),
        h.deps.clone(),
    )
    .await;

    assert!(
        matches!(result.ended, HeadlessEnd::EndTurn),
        "le tour doit se terminer normalement : {:?}",
        result.ended
    );
    assert!(
        result.text.contains("la phrase attendue"),
        "la réponse finale doit être agrégée : {:?}",
        result.text
    );

    // Two composed requests: the second carries the tool result, proof that
    // the turn looped back on the dispatch and did not short-circuit.
    let bodies = h.provider.bodies();
    assert_eq!(bodies.len(), 2, "un aller-retour par tour de modèle");
    let second = serde_json::to_string(&bodies[1]).unwrap();
    assert!(
        second.contains("function_call_output") && second.contains("call_read_1"),
        "la seconde requête doit porter le résultat de l'appel d'outil : {second}"
    );

    // The persisted session is replayable and complete.
    let messages = resumed_messages(&h.session_path);
    assert!(
        orphan_tool_calls(&messages).is_empty(),
        "aucun appel d'outil ne doit rester sans résultat"
    );
    assert!(
        messages.iter().any(|m| m.role == Role::User),
        "le prompt utilisateur doit être persisté"
    );
}

/// AC2: a turn interrupted halfway leaves a valid and resumable
/// session: this is the integrity bug that this PRD fixes, checked here on the
/// full wiring and not on the core in isolation.
#[tokio::test]
async fn interrupted_turn_leaves_a_resumable_session() {
    let workspace = TempWorkspace::new("interrupt");
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();

    let h = harness(
        &workspace,
        [BLOCKING_TURN],
        Some(BlockingTool {
            started: started_tx,
        }),
    );
    let cancel = h.deps.cancel.clone();

    // Interrupts as soon as the tool has really started: the window is that of the
    // dispatch, not a time approximation.
    tokio::spawn(async move {
        let _ = started_rx.recv().await;
        cancel.cancel();
    });

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_headless(
            context("Lance la commande longue", h.tool_specs.clone()),
            h.deps.clone(),
        ),
    )
    .await
    .expect("l'interruption doit rendre la main sans attendre le timeout d'outil");

    assert!(
        matches!(result.ended, HeadlessEnd::EndTurn),
        "une interruption n'est pas une erreur : {:?}",
        result.ended
    );

    let messages = resumed_messages(&h.session_path);
    assert!(
        orphan_tool_calls(&messages).is_empty(),
        "la session reprise ne doit contenir aucun appel d'outil orphelin : {messages:?}"
    );
    let interrupted_result = messages.iter().any(|m| {
        m.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { content, is_error, .. }
                    if *is_error && content.to_lowercase().contains("interrupt")
            )
        })
    });
    assert!(
        interrupted_result,
        "l'appel en vol doit porter un résultat marqué comme interrompu : {messages:?}"
    );

    // Proof of resumability: the repaired transcript goes through the real request
    // builder again without being rejected.
    let replay = ReplayProvider::new([FINAL_TURN]);
    let request = CanonicalRequest {
        model: "gpt-5".to_string(),
        reasoning_effort: None,
        system: None,
        messages,
        tools: Vec::new(),
        max_output_tokens: 4096,
    };
    request
        .validate()
        .expect("le transcript repris doit être un transcript valide");
    let _stream = replay.stream(request).await.unwrap();
    let body = serde_json::to_string(&replay.bodies()[0]).unwrap();
    let calls = body.matches("\"function_call\"").count();
    let outputs = body.matches("\"function_call_output\"").count();
    assert_eq!(
        calls, outputs,
        "la requête de reprise ne doit porter aucun appel d'outil orphelin : {body}"
    );
}

/// AC4: a malformed SSE stream surfaces as a provider contract failure, never
/// as a panic.
#[tokio::test]
async fn malformed_sse_surfaces_as_a_provider_contract_error() {
    let workspace = TempWorkspace::new("malformed");
    let h = harness(&workspace, [MALFORMED_TURN], None);

    let result = run_headless(context("Réponds", Vec::new()), h.deps.clone()).await;

    // The test runs up to here: the malformed stream produced no panic. It remains
    // to check that it does surface as a named provider contract failure, and not
    // as a silent end of turn.
    let outcome = match result.ended {
        HeadlessEnd::Error(err) => err.to_string(),
        other => format!("{other:?}"),
    };
    assert!(
        outcome.contains("function_call done without active call id or name"),
        "un flux malformé doit remonter comme erreur de contrat provider nommée, \
         obtenu : {outcome}"
    );
}

/// AC3: the harness depends neither on a keyring, nor on a terminal, nor
/// on real credentials. Checked structurally: no `Deps` built here
/// carries a credential, and the provider opens no socket.
#[tokio::test]
async fn harness_needs_no_credentials_terminal_or_keyring() {
    let workspace = TempWorkspace::new("no-creds");
    workspace.write("note.txt", "la phrase attendue\n");
    let h = harness(&workspace, [TOOL_CALL_TURN, FINAL_TURN], None);

    let result = run_headless(
        context("Lis note.txt", h.tool_specs.clone()),
        h.deps.clone(),
    )
    .await;
    assert!(matches!(result.ended, HeadlessEnd::EndTurn));

    // The only outgoing contract is the JSON body: it carries no secret.
    let bodies = h.provider.bodies();
    for body in &bodies {
        let rendered = serde_json::to_string(body).unwrap();
        assert!(
            !rendered.contains("Bearer") && !rendered.contains("access_token"),
            "aucune credential ne doit transiter par le corps de requête"
        );
    }
}

// ─────────────────────── US-017: machine event stream ───────────────────────

/// US-017 AC1/AC2/AC5 on the real wiring: every event of a full turn
/// is serializable into ONE parsable JSON line, including when it carries
/// a tool output coming from the disk.
#[tokio::test]
async fn every_event_of_a_full_turn_serializes_to_one_json_line() {
    let workspace = TempWorkspace::new("jsonl");
    // Content hostile to line splitting: newlines, NUL, ANSI.
    workspace.write(
        "note.txt",
        "la phrase attendue\nseconde ligne\u{1b}[31m\u{7}\ttab\n",
    );

    let h = harness(&workspace, [TOOL_CALL_TURN, FINAL_TURN], None);
    let observed = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = Arc::clone(&observed);

    let result = agent_core::run_headless_observed(
        context("Lis note.txt", h.tool_specs.clone()),
        h.deps.clone(),
        move |event| {
            let line = serde_json::to_string(event).unwrap();
            sink.lock().unwrap().push(line);
        },
    )
    .await;

    assert!(matches!(result.ended, HeadlessEnd::EndTurn));
    let lines = observed.lock().unwrap().clone();
    assert_eq!(
        lines.len(),
        result.events,
        "l'observateur doit voir exactement les événements du run"
    );
    for line in &lines {
        assert!(
            !line.contains('\n'),
            "un saut de ligne brut casserait le JSONL : {line}"
        );
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(
            value.get("type").and_then(|t| t.as_str()).is_some(),
            "chaque ligne doit porter son discriminant : {line}"
        );
    }

    // AC3: the model turns are observable and numbered, with monotonic cumulated
    // counters: that is what the end-of-run summary takes back.
    let turns: Vec<serde_json::Value> = lines
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| value["type"] == "model_turn")
        .collect();
    assert_eq!(turns.len(), 2, "deux allers-retours modèle : {turns:?}");
    assert_eq!(turns[0]["data"]["index"], 1);
    assert_eq!(turns[1]["data"]["index"], 2);
    let first_in = turns[0]["data"]["input_tokens"].as_u64().unwrap();
    let second_in = turns[1]["data"]["input_tokens"].as_u64().unwrap();
    assert!(
        second_in >= first_in && second_in > 0,
        "les compteurs sont cumulés : {first_in} puis {second_in}"
    );
}

/// US-017 AC4: the observer does not alter the run. The aggregated text, the number
/// of events and the end cause are identical with and without an observer:
/// that is what guarantees that `--output-format text` stays what it was.
#[tokio::test]
async fn observing_events_does_not_change_the_textual_result() {
    let plain = {
        let workspace = TempWorkspace::new("observe-plain");
        workspace.write("note.txt", "la phrase attendue\n");
        let h = harness(&workspace, [TOOL_CALL_TURN, FINAL_TURN], None);
        run_headless(
            context("Lis note.txt", h.tool_specs.clone()),
            h.deps.clone(),
        )
        .await
    };
    let observed = {
        let workspace = TempWorkspace::new("observe-json");
        workspace.write("note.txt", "la phrase attendue\n");
        let h = harness(&workspace, [TOOL_CALL_TURN, FINAL_TURN], None);
        let seen = Arc::new(Mutex::new(0usize));
        let counter = Arc::clone(&seen);
        let result = agent_core::run_headless_observed(
            context("Lis note.txt", h.tool_specs.clone()),
            h.deps.clone(),
            move |_| *counter.lock().unwrap() += 1,
        )
        .await;
        assert_eq!(*seen.lock().unwrap(), result.events);
        result
    };

    assert_eq!(plain.text, observed.text);
    assert_eq!(plain.events, observed.events);
    assert!(matches!(observed.ended, HeadlessEnd::EndTurn));
}

/// US-018 AC1/AC5: a hook declared as a real process refuses the tool call the
/// model asked for; the refusal reaches the model, the turn goes on, and the
/// persisted session stays replayable. Proves the whole chain (configuration
/// shape -> engine -> pipeline -> transcript), which no unit test sees.
#[tokio::test]
async fn a_hook_vetoes_a_tool_call_without_breaking_the_session() {
    let workspace = TempWorkspace::new("hook-veto");
    workspace.write("note.txt", "la phrase attendue\n");

    let hooks = agent_tools::hooks::CommandHooks::new(
        vec![agent_tools::hooks::HookSpec {
            event: agent_tools::hooks::HookEvent::PreToolUse,
            matcher: Some("read".to_string()),
            command: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                "echo 'lecture interdite par la politique locale' >&2; exit 2".to_string(),
            ],
        }],
        &workspace.path,
    );
    let h = harness_with_hooks(
        &workspace,
        [TOOL_CALL_TURN, FINAL_TURN],
        None,
        Some(Arc::new(hooks)),
    );

    let result = run_headless(
        context("Lis note.txt", h.tool_specs.clone()),
        h.deps.clone(),
    )
    .await;

    assert!(
        matches!(result.ended, HeadlessEnd::EndTurn),
        "le refus d'un hook ne doit pas casser le tour : {:?}",
        result.ended
    );

    // The refusal, and only it, is what the model receives as the tool result.
    let bodies = h.provider.bodies();
    let second = serde_json::to_string(&bodies[1]).unwrap();
    assert!(
        second.contains("lecture interdite par la politique locale"),
        "la raison du hook doit être transmise au modèle : {second}"
    );
    assert!(
        !second.contains("la phrase attendue"),
        "le fichier ne doit pas avoir été lu : {second}"
    );

    // The session stays valid: the refused call has its result, nothing dangles.
    let messages = resumed_messages(&h.session_path);
    assert!(
        orphan_tool_calls(&messages).is_empty(),
        "un appel refusé doit rester apparié à son résultat"
    );
}
