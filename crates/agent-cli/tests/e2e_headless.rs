//! Harness d'intégration bout en bout (US-007, `tasks/prd-harness-parity.md`).
//!
//! Ce que ce harness vérifie, et que les tests unitaires de chaque crate ne
//! peuvent pas voir : le **câblage**. Un tour complet traverse ici le cœur
//! (`agent-core`), le décodeur SSE et le constructeur de requête réels
//! (`agent-provider`), le registre d'outils réel (`agent-tools`) et la
//! persistance JSONL réelle (`agent-session`), sur un répertoire temporaire.
//!
//! Le provider est simulé au niveau du **transport** seulement : il rejoue un
//! flux SSE enregistré (`tests/fixtures/*.sse`) à travers le vrai
//! `CodexEventMapper`, et compose la requête sortante avec le vrai
//! `build_responses_body`. Aucun octet ne part sur le réseau, aucun keyring
//! n'est ouvert, aucun terminal n'est requis (AC3).
//!
//! Le sandbox Landlock n'est PAS posé ici : `restrict_self` est irréversible et
//! s'applique au processus entier, donc l'appliquer confinerait le harness de
//! test lui-même. La frontière de workspace reste vérifiée par la validation de
//! chemin d'`agent-tools`, qui est bien dans le périmètre traversé.

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

// ───────────────────────── Répertoire temporaire ─────────────────────────

/// Répertoire de travail jetable. Écrit dans `$TMPDIR`, supprimé au `Drop` ;
/// aucune dépendance nouvelle pour quinze lignes.
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
        // Le workspace est ancré sur le chemin canonique : `$TMPDIR` est un lien
        // symbolique sur certaines distributions, et la validation de chemin
        // d'`agent-tools` compare des chemins canonicalisés.
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

// ───────────────────────── Provider de rejeu SSE ─────────────────────────

/// Extrait les payloads `data:` d'un flux SSE enregistré, dans l'ordre.
fn sse_payloads(raw: &str) -> Vec<String> {
    raw.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|payload| payload.trim().to_string())
        .filter(|payload| !payload.is_empty())
        .collect()
}

/// Provider simulé : un flux SSE enregistré par tour, décodé par le vrai
/// mapper. Les requêtes composées sont conservées pour être inspectées.
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
        // Chemin de composition RÉEL : c'est lui qui porte le garde-fou des
        // appels d'outils orphelins (US-003). Un transcript cassé échouerait ici,
        // pas dans un simulacre.
        let body = build_responses_body(&req, ResponsesBodyOptions::default());
        self.bodies.lock().unwrap().push(body);

        let Some(raw) = self.turns.lock().unwrap().pop_front() else {
            return Err(ProviderError::Transport(
                "replay provider: no scripted turn left".to_string(),
            ));
        };

        // Décodage RÉEL du flux enregistré, y compris la règle « un flux sans
        // événement terminal est une erreur de contrat » du provider ChatGPT.
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
        // La compaction n'est jamais atteinte par ces scénarios (contexte court).
        Err(ProviderError::Transport(
            "replay provider: complete() is out of scope".to_string(),
        ))
    }

    fn classify_error(&self, _err: &ProviderError) -> ErrorClass {
        // Un flux enregistré ne se répare pas en réessayant : l'échec doit être
        // terminal et déterministe, sinon un test de contrat tournerait en boucle.
        ErrorClass::InvalidRequest
    }
}

// ─────────────────────────── Outil bloquant ───────────────────────────

/// Outil qui signale son démarrage puis ne se termine jamais : donne au test une
/// fenêtre déterministe pour interrompre PENDANT un dispatch, sans dépendre d'un
/// `sleep` arbitraire.
///
/// L'entrée reste un `Value` brut : `agent-cli` ne dépend pas de `serde` en
/// direct, et ce test n'a aucune raison de lui en ajouter une.
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

    fn validate_input(&self, _input: &Self::Input) -> Result<(), ValidationError> {
        Ok(())
    }

    async fn call(&self, _input: Self::Input, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let _ = self.started.send(());
        std::future::pending::<()>().await;
        unreachable!("le dispatch est abandonné avant que cet outil ne rende la main")
    }
}

// ─────────────────────────── Câblage du harness ───────────────────────────

struct Harness {
    provider: Arc<ReplayProvider>,
    deps: Deps,
    session_path: PathBuf,
    /// Specs exposées au modèle, dérivées du registre RÉEL : le test annonce
    /// exactement les outils que le dispatch saura exécuter.
    tool_specs: Vec<agent_core::provider::ToolSpec>,
}

fn harness(
    workspace: &TempWorkspace,
    turns: impl IntoIterator<Item = &'static str>,
    extra_tool: Option<BlockingTool>,
) -> Harness {
    let sessions_dir = workspace.path.join(".pyxis").join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let session_path = sessions_dir.join("e2e.jsonl");
    let session = JsonlSession::create_at(&session_path).unwrap();

    let mut builder = Registry::builder(&workspace.path).register(Read);
    if let Some(tool) = extra_tool {
        builder = builder.register(tool);
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
    }
}

/// Relit la session persistée et retourne les messages rejoués, comme le ferait
/// `pyxis --resume`.
fn resumed_messages(path: &std::path::Path) -> Vec<Message> {
    agent_session::resume_file(path).unwrap().messages
}

/// Appels d'outils sans résultat correspondant dans un transcript.
fn orphan_tool_calls(messages: &[Message]) -> Vec<String> {
    agent_core::message::unanswered_tool_calls(messages)
}

// ───────────────────────────────── Tests ─────────────────────────────────

/// AC1 : un tour complet — appel d'outil réel puis réponse finale — se déroule
/// de bout en bout sur un répertoire temporaire, sans réseau.
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

    // Deux requêtes composées : la seconde porte le résultat d'outil, preuve que
    // le tour a bouclé sur le dispatch et non court-circuité.
    let bodies = h.provider.bodies();
    assert_eq!(bodies.len(), 2, "un aller-retour par tour de modèle");
    let second = serde_json::to_string(&bodies[1]).unwrap();
    assert!(
        second.contains("function_call_output") && second.contains("call_read_1"),
        "la seconde requête doit porter le résultat de l'appel d'outil : {second}"
    );

    // La session persistée est rejouable et complète.
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

/// AC2 : un tour interrompu à mi-parcours laisse une session valide et
/// reprenable — c'est le bug d'intégrité que ce PRD corrige, vérifié ici sur le
/// câblage complet et non sur le cœur isolé.
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

    // Interrompt dès que l'outil a réellement démarré : la fenêtre est celle du
    // dispatch, pas une approximation temporelle.
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

    // Preuve de reprise : le transcript réparé repasse par le constructeur de
    // requête réel sans être rejeté.
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

/// AC4 : un flux SSE malformé remonte comme échec de contrat provider, jamais
/// comme panique.
#[tokio::test]
async fn malformed_sse_surfaces_as_a_provider_contract_error() {
    let workspace = TempWorkspace::new("malformed");
    let h = harness(&workspace, [MALFORMED_TURN], None);

    let result = run_headless(context("Réponds", Vec::new()), h.deps.clone()).await;

    // Le test tourne jusqu'ici : le flux malformé n'a produit aucune panique. Reste
    // à vérifier qu'il remonte bien comme échec de contrat provider nommé, et non
    // comme une fin de tour silencieuse.
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

/// AC3 : le harness ne dépend ni d'un keyring, ni d'un terminal, ni
/// d'identifiants réels. Vérifié structurellement : aucun `Deps` construit ici
/// ne porte de credential, et le provider n'ouvre aucune socket.
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

    // Le seul contrat sortant est le corps JSON : il ne porte aucun secret.
    let bodies = h.provider.bodies();
    for body in &bodies {
        let rendered = serde_json::to_string(body).unwrap();
        assert!(
            !rendered.contains("Bearer") && !rendered.contains("access_token"),
            "aucune credential ne doit transiter par le corps de requête"
        );
    }
}
