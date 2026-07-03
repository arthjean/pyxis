//! Boucle interactive : assemble le frontend (`agent-tui`), le stream d'agent
//! (`agent-core`) et les demandes de permission en un `tokio::select`.
//!
//! - Les frappes clavier arrivent d'un thread dédié (crossterm `read()` bloque).
//! - Chaque soumission spawn `run_agent` ; ses `AgentEvent` reviennent par mpsc.
//! - Une demande de permission suspend le pipeline d'outils jusqu'à la réponse
//!   utilisateur (le dialog ne fige PAS la boucle : le select continue de rendre
//!   et de lire le clavier).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use agent_core::message::{ContentBlock, Message, recent_untrusted_content};
use agent_core::provider::ToolSpec;
use agent_core::{AgentContext, AgentEvent, Deps, RunConfig, Session, run_agent};
use agent_provider::KEYRING_ACCOUNT;
use agent_tui::{
    AppState, Block, COMMANDS, InputAction, McpServerMeta, McpStatus, SessionMeta,
    blocks_from_messages,
};
use crossterm::event::{Event, KeyEventKind, MouseEventKind};
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};

use crate::approver::{PermissionMsg, to_prompt};
use crate::session::SharedSession;

/// Nombre maximal d'entrées d'historique de prompts agrégées par dossier.
const PROMPT_HISTORY_CAP: usize = 200;

/// Résultat d'une connexion MCP lancée en tâche de fond. Revient dans la boucle
/// `select!` pour mettre à jour le registre et l'affichage sans figer le TUI.
enum McpEvent {
    Connected {
        name: String,
        conn: agent_mcp::McpConnection,
        tools: Vec<agent_mcp::McpToolInfo>,
    },
    Failed {
        name: String,
        error: String,
    },
}

pub struct InteractiveConfig {
    pub model: String,
    /// Guidelines comportementales des outils (US-026), injectées dans le system
    /// prompt. Stockées brutes (pas pré-composées) car le system de base dépend du
    /// slug courant (US-027) et est recomposé par tour.
    pub tool_guidelines: Vec<String>,
    /// Contexte projet éphémère (AGENTS.md + env, US-028), ré-injecté à chaque tour
    /// dans `AgentContext::context_messages` (jamais persisté).
    pub context_messages: Vec<Message>,
    pub run_config: RunConfig,
    pub tool_specs: Vec<ToolSpec>,
    pub truecolor: bool,
    /// Reduced-motion (`NO_COLOR` / `PYXIS_REDUCED_MOTION`) : spinner dégradé en
    /// point pulsé plutôt qu'animé (US-044).
    pub reduced_motion: bool,
    /// Credential du fournisseur présente (badge connecté + sous-menu providers).
    pub connected: bool,
    /// Skills disponibles (lus avant le sandbox), sous-menu `/skills`.
    pub skills: Vec<String>,
    /// Objectif de session persistant (`/goal`), composé dans le system à chaque
    /// tour. Chargé du sidecar `.pyxis/goal` au démarrage (survit au redémarrage).
    pub goal: Option<String>,
    /// Durcissement appliqué aux sous-process MCP (env scrub + proxy).
    pub command_hardener: agent_tools::CommandHardener,
}

/// Marqueur de complétion émis par le modèle quand l'objectif est pleinement
/// atteint. Détecté par le harness pour auto-effacer le goal ; strippé de l'affichage.
pub const GOAL_DONE_MARKER: &str = "<<GOAL_DONE>>";

/// Garde-fou : nombre max de relances automatiques par objectif (anti-runaway).
const MAX_GOAL_ITERS: u32 = 25;

/// Message injecté à chaque relance automatique tant que l'objectif n'est pas marqué atteint.
const GOAL_CONTINUE_PROMPT: &str = "Poursuis l'objectif de session. S'il reste \
    du travail, continue. S'il est pleinement atteint et vérifié, termine ta \
    réponse par <<GOAL_DONE>> seul sur la dernière ligne.";

const GOAL_ITERS_FILE: &str = "goal.iters";

/// Compose le system prompt effectif : base + DIRECTIVE de complétion. L'objectif
/// vit dans `instructions` (re-envoyé chaque tour) donc survit à la compaction —
/// `agent-core::compaction` ne touche que `messages`, jamais le system.
pub fn compose_system(base: &str, goal: Option<&str>) -> String {
    match goal {
        Some(g) if !g.trim().is_empty() => format!(
            "{base}\n\n\
             ## Objectif à accomplir — NE T'ARRÊTE PAS avant qu'il soit PLEINEMENT atteint\n\
             {g}\n\n\
             Travaille en continu (lis, édite, exécute) jusqu'à ce que cet objectif soit \
             ENTIÈREMENT accompli, sans demander de confirmation. Tant qu'il reste quoi que ce \
             soit à faire, continue. Quand — et SEULEMENT quand — l'objectif est pleinement \
             atteint et vérifié, termine ta toute dernière réponse par le marqueur exact, seul \
             sur sa dernière ligne :\n{GOAL_DONE_MARKER}\n\
             N'écris JAMAIS ce marqueur tant que l'objectif n'est pas pleinement atteint."
        ),
        _ => base.to_string(),
    }
}

/// Injecte les guidelines comportementales des outils (US-026) dans le system
/// prompt sous une section dédiée. Appelé UNE fois au démarrage (les outils sont
/// fixes) pour produire la base que `compose_system` enrichit ensuite par tour.
/// Sans guideline, renvoie la base inchangée (pas de section vide).
pub fn with_tool_guidelines(base: &str, guidelines: &[String]) -> String {
    if guidelines.is_empty() {
        return base.to_string();
    }
    let mut s = String::from(base);
    s.push_str("\n\n## Règles d'utilisation des outils\n");
    for g in guidelines {
        s.push_str("- ");
        s.push_str(g);
        s.push('\n');
    }
    s.truncate(s.trim_end().len());
    s
}

/// Construit le contexte du tour (conversation à jour + message) et lance
/// `run_agent` dans une tâche dont les events reviennent par `tx`.
fn launch_turn(
    conversation: &Arc<Mutex<Vec<Message>>>,
    cfg: &InteractiveConfig,
    deps: &Deps,
    tx: &mpsc::Sender<AgentEvent>,
    user_msg: &str,
) {
    let mut msgs = conversation.lock().map(|g| g.clone()).unwrap_or_default();
    msgs.push(Message::user(user_msg.to_string()));
    // US-027 : system de base sélectionné par le slug COURANT (recalculé par tour →
    // un `/models` change le template) + guidelines outils + directive d'objectif.
    let base = with_tool_guidelines(
        crate::prompt::select_system_prompt(&cfg.model),
        &cfg.tool_guidelines,
    );
    let ctx = AgentContext {
        model: cfg.model.clone(),
        system: Some(compose_system(&base, cfg.goal.as_deref())),
        messages: msgs,
        tools: cfg.tool_specs.clone(),
        config: cfg.run_config.clone(),
        // US-028 : contexte projet ré-injecté chaque tour, jamais persisté.
        context_messages: cfg.context_messages.clone(),
    };
    let deps = deps.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let stream = run_agent(ctx, deps);
        futures_util::pin_mut!(stream);
        while let Some(ev) = stream.next().await {
            if tx.send(ev).await.is_err() {
                break;
            }
        }
    });
}

/// Si la dernière réponse de l'assistant porte le marqueur de complétion, le
/// retire de l'affichage et retourne `true` (objectif atteint).
fn take_goal_done(state: &mut AppState) -> bool {
    for block in state.blocks.iter_mut().rev() {
        if let Block::Assistant { text, .. } = block {
            let trimmed = text.trim_end();
            let marker_is_last_line = trimmed
                .lines()
                .next_back()
                .is_some_and(|line| line.trim() == GOAL_DONE_MARKER);
            if marker_is_last_line {
                let mut lines: Vec<&str> = trimmed.lines().collect();
                if lines
                    .last()
                    .is_some_and(|line| line.trim() == GOAL_DONE_MARKER)
                {
                    lines.pop();
                }
                *text = lines.join("\n").trim_end().to_string();
                return true;
            }
            return false;
        }
    }
    false
}

pub(crate) fn session_path_from_arg(sessions_dir: &Path, arg: &str) -> Option<PathBuf> {
    let candidate = Path::new(arg);
    if arg.trim().is_empty()
        || candidate.components().count() != 1
        || candidate.extension().and_then(|e| e.to_str()) != Some("jsonl")
    {
        return None;
    }
    Some(sessions_dir.join(candidate))
}

fn read_goal_iters(path: Option<&Path>) -> u32 {
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

fn write_goal_iters(path: Option<&Path>, value: u32) -> std::io::Result<()> {
    if let Some(path) = path {
        std::fs::write(path, value.to_string())?;
    }
    Ok(())
}

fn remove_if_exists(path: Option<&Path>) -> std::io::Result<()> {
    if let Some(path) = path {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn scrub_encrypted_reasoning(messages: &mut [Message]) -> usize {
    let mut removed = 0usize;
    for msg in messages {
        let before = msg.content.len();
        msg.content
            .retain(|b| !matches!(b, ContentBlock::EncryptedReasoning { .. }));
        removed += before.saturating_sub(msg.content.len());
    }
    removed
}

fn count_encrypted_reasoning(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|msg| {
            msg.content
                .iter()
                .filter(|b| matches!(b, ContentBlock::EncryptedReasoning { .. }))
                .count()
        })
        .sum()
}

/// Lance la session interactive. Restaure le terminal en sortie quoi qu'il arrive.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    deps: Deps,
    conversation: Arc<Mutex<Vec<Message>>>,
    perm_rx: mpsc::Receiver<PermissionMsg>,
    cfg: InteractiveConfig,
    session: Arc<SharedSession>,
    sessions_dir: PathBuf,
    current_session: PathBuf,
    mcp: Arc<Mutex<agent_mcp::McpRegistry>>,
) -> anyhow::Result<()> {
    let mut tui = agent_tui::enter()?;
    let result = event_loop(
        &mut tui,
        deps,
        conversation,
        perm_rx,
        cfg,
        session,
        sessions_dir,
        current_session,
        mcp,
    )
    .await;
    agent_tui::leave(&mut tui)?;
    result
}

#[allow(clippy::too_many_arguments)]
async fn event_loop(
    tui: &mut agent_tui::Tui,
    deps: Deps,
    conversation: Arc<Mutex<Vec<Message>>>,
    mut perm_rx: mpsc::Receiver<PermissionMsg>,
    mut cfg: InteractiveConfig,
    session: Arc<SharedSession>,
    sessions_dir: PathBuf,
    mut current_session: PathBuf,
    mcp: Arc<Mutex<agent_mcp::McpRegistry>>,
) -> anyhow::Result<()> {
    let mut state = AppState::new(cfg.model.clone(), cfg.truecolor);
    state.workspace = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    state.provider_connected = cfg.connected;
    state.reduced_motion = cfg.reduced_motion;
    state.skills = std::mem::take(&mut cfg.skills);
    state.sessions = load_sessions(&sessions_dir, &current_session);
    state.mcp_servers = mcp_metas(&mcp);
    // Sidecar de l'objectif persistant (`<workspace>/.pyxis/goal`).
    let goal_path = sessions_dir.parent().map(|p| p.join("goal"));
    let goal_iters_path = sessions_dir.parent().map(|p| p.join(GOAL_ITERS_FILE));
    // Historique des prompts de TOUT le dossier (toutes les conversations).
    state.load_history(agent_session::workspace_prompts(
        &sessions_dir,
        Some(&current_session),
        PROMPT_HISTORY_CAP,
    ));
    let initial_messages = conversation.lock().map(|g| g.clone()).unwrap_or_default();
    if !initial_messages.is_empty() {
        state.blocks = blocks_from_messages(&initial_messages);
        state.blocks.push(Block::Notice(format!(
            "Session reprise ({} messages).",
            initial_messages.len()
        )));
    }
    // Transcript vide au démarrage → l'écran d'accueil (carte + logo) s'affiche
    // de lui-même (cf. `AppState::is_welcome`), pas de Notice à pousser.

    // Thread lecteur clavier → mpsc (crossterm read() est bloquant).
    let (key_tx, mut key_rx) = mpsc::channel::<Event>(64);
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if key_tx.blocking_send(ev).is_err() {
                break;
            }
        }
    });

    let (agent_tx, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
    let (mcp_tx, mut mcp_rx) = mpsc::channel::<McpEvent>(16);
    let mut running = false;
    // Compteur de relances automatiques de la boucle d'objectif (reset à chaque
    // saisie utilisateur / nouvel objectif).
    let mut goal_iters: u32 = if cfg.goal.is_some() {
        read_goal_iters(goal_iters_path.as_deref())
    } else {
        0
    };
    let mut pending_resp: Option<oneshot::Sender<bool>> = None;

    // Tick d'animation du spinner (US-044). 100 ms ≈ 10 fps : fluide et quasi gratuit
    // (le cache de rendu sert les blocs bakés). `Skip` évite tout burst de redraw au
    // retour d'idle. La branche `select!` est gardée par `if running` → 0 CPU en idle.
    let mut spinner = tokio::time::interval(Duration::from_millis(100));
    spinner.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Début du tour courant (front montant de `running`) pour la durée écoulée.
    let mut turn_start: Option<Instant> = None;

    loop {
        // Front montant/descendant de `running` : démarre / fige le suivi de
        // progression (spinner, durée, tokens). N'altère PAS l'orchestration.
        match (running, turn_start.is_some()) {
            (true, false) => {
                turn_start = Some(Instant::now());
                state.begin_turn();
            }
            (false, true) => {
                turn_start = None;
                state.end_turn();
            }
            _ => {}
        }
        tui.draw(|f| agent_tui::render(f, &state))?;
        if state.should_quit {
            break;
        }

        tokio::select! {
            ev = key_rx.recv() => {
                let k = match ev {
                    None => break, // canal d'événements fermé → on sort
                    Some(Event::Mouse(m)) => {
                        // molette → scroll du transcript (capture souris activée).
                        match m.kind {
                            MouseEventKind::ScrollUp => state.scroll_up(3),
                            MouseEventKind::ScrollDown => state.scroll_down(3),
                            _ => {}
                        }
                        continue;
                    }
                    // frappe normale ; on ignore les répétitions de relâche.
                    Some(Event::Key(k)) if k.kind != KeyEventKind::Release => k,
                    Some(_) => continue, // key release, resize… → simple redraw
                };
                match state.on_key(k) {
                    InputAction::Submit(prompt) if !running => {
                        state.push_user(prompt.clone());
                        goal_iters = 0;
                        launch_turn(&conversation, &cfg, &deps, &agent_tx, &prompt);
                        running = true;
                    }
                    InputAction::Command(line) => {
                        let mut it = line.splitn(2, ' ');
                        let cmd = it.next().unwrap_or("");
                        let arg = it.next().unwrap_or("").trim();
                        match cmd {
                            "/help" => {
                                let list = COMMANDS
                                    .iter()
                                    .map(|(n, _, _)| *n)
                                    .collect::<Vec<_>>()
                                    .join("  ");
                                state.blocks.push(Block::Notice(format!("Commandes : {list}")));
                            }
                            "/models" => {
                                if arg.is_empty() {
                                    state.blocks.push(Block::Notice(
                                        "Usage : /models <slug> (ex: /models gpt-5.5)".into(),
                                    ));
                                } else {
                                    let removed = conversation
                                        .lock()
                                        .map(|msgs| count_encrypted_reasoning(&msgs[..]))
                                        .unwrap_or_default();
                                    if removed > 0
                                        && let Err(e) = session.redact_encrypted_reasoning().await
                                    {
                                        state.blocks.push(Block::Error(format!(
                                            "models: redaction reasoning: {e}"
                                        )));
                                        continue;
                                    }
                                    if removed > 0 {
                                        let _ = conversation
                                            .lock()
                                            .map(|mut msgs| scrub_encrypted_reasoning(&mut msgs[..]));
                                    }
                                    cfg.model = arg.to_string();
                                    state.model = arg.to_string();
                                    let suffix = if removed > 0 {
                                        format!(" ({removed} reasoning items retirés)")
                                    } else {
                                        String::new()
                                    };
                                    state
                                        .blocks
                                        .push(Block::Notice(format!("Modèle : {arg}{suffix}")));
                                }
                            }
                            "/goal" if running => state.blocks.push(Block::Notice(
                                "Attends la fin du tour en cours.".into(),
                            )),
                            "/goal" => match arg {
                                "" => state.blocks.push(Block::Notice(match &cfg.goal {
                                    Some(g) => format!("Objectif actif : {g}"),
                                    None => "Aucun objectif. Usage : /goal <objectif à accomplir>".into(),
                                })),
                                "clear" => {
                                    cfg.goal = None;
                                    if let Err(e) = remove_if_exists(goal_path.as_deref()) {
                                        state.blocks.push(Block::Error(format!("goal: {e}")));
                                    }
                                    if let Err(e) = remove_if_exists(goal_iters_path.as_deref()) {
                                        state.blocks.push(Block::Error(format!("goal: {e}")));
                                    }
                                    state.blocks.push(Block::Notice("Objectif effacé.".into()));
                                }
                                g => {
                                    // Fixe l'objectif (sidecar : survit redémarrage + /resume)
                                    // ET lance immédiatement le travail vers lui.
                                    cfg.goal = Some(g.to_string());
                                    if let Some(p) = &goal_path
                                        && let Err(e) = std::fs::write(p, g)
                                    {
                                        state.blocks.push(Block::Error(format!("goal: {e}")));
                                    }
                                    if let Err(e) = write_goal_iters(goal_iters_path.as_deref(), 0) {
                                        state.blocks.push(Block::Error(format!("goal: {e}")));
                                    }
                                    goal_iters = 0;
                                    state.push_user(g);
                                    launch_turn(&conversation, &cfg, &deps, &agent_tx, g);
                                    running = true;
                                }
                            },
                            // resume / new / clear pendant un tour : on attend (le
                            // fichier de persistance est en cours d'écriture par le stream).
                            "/resume" | "/new" | "/clear" if running => {
                                state.blocks.push(Block::Notice(
                                    "Attends la fin du tour en cours.".into(),
                                ));
                            }
                            "/resume" => {
                                let Some(path) = session_path_from_arg(&sessions_dir, arg) else {
                                    state.blocks.push(Block::Error(
                                        "resume: identifiant de session invalide".into(),
                                    ));
                                    continue;
                                };
                                match agent_session::resume_file(&path) {
                                    Ok(r) if !r.messages.is_empty() => {
                                        let msgs = r.messages;
                                        if let Err(e) = session.switch_file(&path, msgs.len()) {
                                            state.blocks.push(Block::Error(format!("resume: {e}")));
                                        } else {
                                            current_session = path;
                                            if let Ok(mut g) = conversation.lock() {
                                                *g = msgs.clone();
                                            }
                                            deps.tools.seed_taint(recent_untrusted_content(
                                                &msgs,
                                                crate::RESUME_TAINT_SCAN_MESSAGES,
                                            ));
                                            state.blocks = blocks_from_messages(&msgs);
                                            // L'historique reste global au dossier (déjà chargé).
                                            state.blocks.push(Block::Notice(format!(
                                                "Session reprise ({} messages).",
                                                msgs.len()
                                            )));
                                            state.sessions =
                                                load_sessions(&sessions_dir, &current_session);
                                        }
                                    }
                                    Ok(_) => state
                                        .blocks
                                        .push(Block::Notice("Session vide.".into())),
                                    Err(e) => {
                                        state.blocks.push(Block::Error(format!("resume: {e}")))
                                    }
                                }
                            }
                            // /clear est un alias de /new : même mécanique (nouveau
                            // fichier de session + contexte vidé), seul le libellé change.
                            // L'objectif (`cfg.goal`) survit, comme le system prompt.
                            "/new" | "/clear" => {
                                let path = new_session_path(&sessions_dir);
                                if let Err(e) = session.switch_file(&path, 0) {
                                    state.blocks.push(Block::Error(format!("{cmd}: {e}")));
                                } else {
                                    current_session = path;
                                    if let Ok(mut g) = conversation.lock() {
                                        g.clear();
                                    }
                                    // Transcript vidé → l'écran d'accueil réapparaît,
                                    // ce qui sert de confirmation visuelle (pas de Notice).
                                    state.blocks.clear();
                                    state.sessions =
                                        load_sessions(&sessions_dir, &current_session);
                                }
                            }
                            "/providers" => match arg {
                                "apikey" => state.blocks.push(Block::Notice(
                                    "L'authentification par clé API arrive bientôt.".into(),
                                )),
                                "subscription anthropic" => state.blocks.push(Block::Notice(
                                    "Anthropic (Claude Pro/Max) arrive bientôt.".into(),
                                )),
                                "subscription codex connect" => {
                                    if state.provider_connected {
                                        state
                                            .blocks
                                            .push(Block::Notice("Déjà connecté à Codex.".into()));
                                    } else {
                                        state.blocks.push(Block::Notice(
                                            "Reconnexion : relance le login — \
                                             cargo run -p agent-auth --example login"
                                                .into(),
                                        ));
                                    }
                                }
                                "subscription codex disconnect" => {
                                    if state.provider_connected {
                                        if let Err(e) = agent_auth::store::delete(KEYRING_ACCOUNT) {
                                            state
                                                .blocks
                                                .push(Block::Error(format!("déconnexion : {e}")));
                                        } else if let Err(e) = deps.provider.disconnect_auth().await {
                                            state.blocks.push(Block::Error(format!(
                                                "déconnexion provider : {e}"
                                            )));
                                        } else {
                                                state.provider_connected = false;
                                                state.blocks.push(Block::Notice(
                                                    "Déconnecté de Codex (credential supprimée). \
                                                     Relance le login avant le prochain appel modèle."
                                                        .into(),
                                                ));
                                        }
                                    } else {
                                        state
                                            .blocks
                                            .push(Block::Notice("Déjà déconnecté.".into()));
                                    }
                                }
                                "" | "subscription" | "subscription codex" => {
                                    state.blocks.push(Block::Notice(
                                        "Choisis un fournisseur puis une action dans le sous-menu."
                                            .into(),
                                    ))
                                }
                                other => state
                                    .blocks
                                    .push(Block::Notice(format!("Fournisseur inconnu : {other}"))),
                            },
                            "/mcp" => {
                                handle_mcp(arg, &mcp, &mcp_tx, &cfg.command_hardener, &mut state)
                            }
                            "/skills" => state.blocks.push(Block::Notice(
                                "Choisis un skill dans le sous-menu /skills.".into(),
                            )),
                            "/quit" => state.should_quit = true,
                            other => state
                                .blocks
                                .push(Block::Notice(format!("Commande inconnue : {other}"))),
                        }
                        state.scroll = 0;
                    }
                    InputAction::Quit => state.should_quit = true,
                    InputAction::Permission(allow) => {
                        if let Some(resp) = pending_resp.take() {
                            let _ = resp.send(allow);
                        }
                    }
                    _ => {}
                }
            }
            ev = agent_rx.recv(), if running => {
                if let Some(ev) = ev {
                    let endturn = matches!(ev, AgentEvent::EndTurn);
                    let stop = matches!(
                        ev,
                        AgentEvent::EndTurn | AgentEvent::Error(_) | AgentEvent::Exhausted(_)
                    );
                    state.apply(&ev);
                    if stop {
                        // Boucle d'objectif : sur un EndTurn « propre » avec un goal
                        // actif, on relance tant que le marqueur de complétion n'est
                        // pas émis (le modèle ne décide pas seul de s'arrêter).
                        if endturn && cfg.goal.is_some() {
                            if take_goal_done(&mut state) {
                                cfg.goal = None;
                                if let Err(e) = remove_if_exists(goal_path.as_deref()) {
                                    state.blocks.push(Block::Error(format!("goal: {e}")));
                                }
                                if let Err(e) = remove_if_exists(goal_iters_path.as_deref()) {
                                    state.blocks.push(Block::Error(format!("goal: {e}")));
                                }
                                state
                                    .blocks
                                    .push(Block::Notice("✓ Objectif atteint — effacé.".into()));
                                running = false;
                            } else if goal_iters < MAX_GOAL_ITERS {
                                goal_iters += 1;
                                if let Err(e) =
                                    write_goal_iters(goal_iters_path.as_deref(), goal_iters)
                                {
                                    state.blocks.push(Block::Error(format!("goal: {e}")));
                                    running = false;
                                    continue;
                                }
                                state.blocks.push(Block::Notice(format!(
                                    "↻ poursuite de l'objectif ({goal_iters}/{MAX_GOAL_ITERS})…"
                                )));
                                launch_turn(
                                    &conversation,
                                    &cfg,
                                    &deps,
                                    &agent_tx,
                                    GOAL_CONTINUE_PROMPT,
                                );
                                // running reste true : un nouveau tour est lancé.
                            } else {
                                state.blocks.push(Block::Notice(format!(
                                    "Objectif non confirmé après {MAX_GOAL_ITERS} relances — \
                                     arrêt. /goal clear pour abandonner."
                                )));
                                running = false;
                            }
                        } else {
                            running = false;
                        }
                    }
                }
            }
            perm = perm_rx.recv() => {
                if let Some((req, resp)) = perm {
                    state.pending = Some(to_prompt(&req));
                    pending_resp = Some(resp);
                }
            }
            ev = mcp_rx.recv() => {
                if let Some(ev) = ev {
                    match ev {
                        McpEvent::Connected { name, conn, tools } => {
                            let n = tools.len();
                            // Verrou empoisonné → on ferme la connexion au lieu de la
                            // dropper en silence (sinon sous-process orphelin).
                            match mcp.lock() {
                                Ok(mut r) => {
                                    if let Some(c) = r.finish_connect(&name, conn, tools) {
                                        // Déconnecté pendant la connexion → session orpheline.
                                        tokio::spawn(async move { c.cancel().await });
                                        state.blocks.push(Block::Notice(format!(
                                            "MCP « {name} » : connexion annulée."
                                        )));
                                    } else {
                                        state.blocks.push(Block::Notice(format!(
                                            "✓ MCP « {name} » connecté ({n} outils)."
                                        )));
                                    }
                                }
                                Err(_) => {
                                    tokio::spawn(async move { conn.cancel().await });
                                    state.blocks.push(Block::Error(
                                        "MCP : registre indisponible — connexion fermée.".into(),
                                    ));
                                }
                            }
                        }
                        McpEvent::Failed { name, error } => {
                            if let Ok(mut r) = mcp.lock() {
                                r.fail(&name, error.clone());
                            }
                            state
                                .blocks
                                .push(Block::Error(format!("✗ MCP « {name} » : {error}")));
                        }
                    }
                    state.mcp_servers = mcp_metas(&mcp);
                }
            }
            // Tick d'animation : réveille la boucle UNIQUEMENT pendant un tour ACTIF
            // (`if running`) et hors attente de permission (`pending.is_none()` : on
            // attend l'humain, pas l'agent → 0 CPU, pas de redraw à 10 fps du dialog).
            // Le redraw a lieu en tête de boucle ; ici on ne fait qu'avancer l'état
            // d'animation (spinner, durée). US-044.
            _ = spinner.tick(), if running && state.pending.is_none() => {
                let elapsed = turn_start.map(|t| t.elapsed()).unwrap_or_default();
                state.tick_progress(elapsed);
            }
        }
    }
    Ok(())
}

/// Traite `/mcp [<serveur> <action>]`. Les connexions (spawn + handshake) sont
/// lancées en tâche de fond — le résultat revient par `mcp_tx` dans la boucle.
fn handle_mcp(
    arg: &str,
    mcp: &Arc<Mutex<agent_mcp::McpRegistry>>,
    mcp_tx: &mpsc::Sender<McpEvent>,
    command_hardener: &agent_tools::CommandHardener,
    state: &mut AppState,
) {
    let Some((server, action)) = arg.rsplit_once(' ') else {
        state.blocks.push(Block::Notice(
            "Sélectionne un serveur puis une action dans le sous-menu /mcp.".into(),
        ));
        return;
    };
    let server = server.trim();
    if server.is_empty() {
        state
            .blocks
            .push(Block::Notice("Usage : /mcp <serveur> <action>.".into()));
        return;
    }
    match action {
        "connect" | "reconnect" => {
            let begin = match mcp.lock() {
                Ok(mut r) => r.begin_connect(server),
                Err(_) => Err(agent_mcp::McpError::Unknown(server.to_string())),
            };
            match begin {
                Ok((cfg_srv, old)) => {
                    if let Some(old) = old {
                        tokio::spawn(async move { old.cancel().await });
                    }
                    state.mcp_servers = mcp_metas(mcp);
                    state
                        .blocks
                        .push(Block::Notice(format!("MCP « {server} » : connexion…")));
                    let tx = mcp_tx.clone();
                    let name = server.to_string();
                    let harden = Arc::clone(command_hardener);
                    tokio::spawn(async move {
                        let ev = match agent_mcp::McpConnection::connect_hardened(
                            &name,
                            &cfg_srv,
                            Some(&harden),
                        )
                        .await
                        {
                            Ok(conn) => match conn.list_tools(&name).await {
                                Ok(tools) => McpEvent::Connected { name, conn, tools },
                                Err(e) => {
                                    conn.cancel().await;
                                    McpEvent::Failed {
                                        name,
                                        error: e.to_string(),
                                    }
                                }
                            },
                            Err(e) => McpEvent::Failed {
                                name,
                                error: e.to_string(),
                            },
                        };
                        // Canal fermé (arrêt de l'app) → on récupère la connexion et
                        // on la ferme pour ne pas laisser de sous-process orphelin.
                        if let Err(mpsc::error::SendError(ev)) = tx.send(ev).await
                            && let McpEvent::Connected { conn, .. } = ev
                        {
                            conn.cancel().await;
                        }
                    });
                }
                Err(e) => state.blocks.push(Block::Notice(format!("MCP : {e}"))),
            }
        }
        "disconnect" => {
            let old = mcp.lock().ok().and_then(|mut r| r.begin_disconnect(server));
            match old {
                Some(old) => {
                    tokio::spawn(async move { old.cancel().await });
                    state
                        .blocks
                        .push(Block::Notice(format!("MCP « {server} » déconnecté.")));
                }
                None => state
                    .blocks
                    .push(Block::Notice(format!("MCP « {server} » non connecté."))),
            }
            state.mcp_servers = mcp_metas(mcp);
        }
        "tools" => {
            if let Ok(reg) = mcp.lock() {
                match reg.get(server) {
                    Some(s) if !s.tools().is_empty() => {
                        let names = s
                            .tools()
                            .iter()
                            .map(|t| t.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        state.blocks.push(Block::Notice(format!(
                            "MCP « {server} » ({} outils) : {names}",
                            s.tools().len()
                        )));
                    }
                    Some(_) => state.blocks.push(Block::Notice(format!(
                        "MCP « {server} » : aucun outil exposé."
                    ))),
                    None => state
                        .blocks
                        .push(Block::Notice(format!("MCP « {server} » inconnu."))),
                }
            }
        }
        other => state
            .blocks
            .push(Block::Notice(format!("Action MCP inconnue : {other}"))),
    }
}

/// Projette le registre MCP en métadonnées d'affichage pour le sous-menu `/mcp`.
fn mcp_metas(mcp: &Arc<Mutex<agent_mcp::McpRegistry>>) -> Vec<McpServerMeta> {
    let Ok(reg) = mcp.lock() else {
        return Vec::new();
    };
    reg.iter()
        .map(|(name, server)| McpServerMeta {
            name: name.clone(),
            status: match server {
                agent_mcp::McpServer::Disconnected { .. } => McpStatus::Disconnected,
                agent_mcp::McpServer::Connecting { .. } => McpStatus::Connecting,
                agent_mcp::McpServer::Connected { .. } => McpStatus::Connected,
                agent_mcp::McpServer::Failed { .. } => McpStatus::Failed,
            },
            tool_count: server.tool_count(),
        })
        .collect()
}

/// Charge les sessions reprenables (8 plus récentes) en items de menu, en
/// excluant la session courante. Le libellé = 1re ligne du 1er message.
fn load_sessions(dir: &Path, exclude: &Path) -> Vec<SessionMeta> {
    agent_session::list_sessions(dir, Some(exclude))
        .into_iter()
        .take(8)
        .map(|s| {
            let label = match s.summary.lines().next().map(str::trim) {
                Some(l) if !l.is_empty() => l.to_string(),
                _ => "(sans titre)".to_string(),
            };
            SessionMeta {
                id: s.id,
                label,
                hint: format!("{} msg · {}", s.message_count, relative_time(s.modified)),
            }
        })
        .collect()
}

/// Âge lisible d'une session (« il y a 3 min »).
fn relative_time(modified: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if secs < 60 {
        format!("il y a {secs}s")
    } else if secs < 3_600 {
        format!("il y a {} min", secs / 60)
    } else if secs < 86_400 {
        format!("il y a {} h", secs / 3_600)
    } else {
        format!("il y a {} j", secs / 86_400)
    }
}

/// Chemin d'un nouveau fichier de session (horodaté, un par conversation).
pub(crate) fn new_session_path(dir: &Path) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    for seq in 0..1000 {
        let name = if seq == 0 {
            format!("{millis}.jsonl")
        } else {
            format!("{millis}-{seq}.jsonl")
        };
        let path = dir.join(name);
        if !path.exists() {
            return path;
        }
    }
    dir.join(format!("{millis}-overflow.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::{
        GOAL_DONE_MARKER, compose_system, count_encrypted_reasoning, scrub_encrypted_reasoning,
        session_path_from_arg, take_goal_done,
    };
    use agent_core::message::{ContentBlock, Message};
    use agent_tui::{AppState, Block};
    use std::path::Path;

    #[test]
    fn compose_system_pins_completion_directive() {
        let base = "Tu es Pyxis.";
        assert_eq!(compose_system(base, None), base);
        assert_eq!(
            compose_system(base, Some("   ")),
            base,
            "objectif vide → base"
        );
        let with = compose_system(base, Some("refonds l'UI"));
        assert!(with.starts_with(base));
        assert!(with.contains("NE T'ARRÊTE PAS"), "directive de complétion");
        assert!(with.contains(GOAL_DONE_MARKER), "marqueur instruit");
        assert!(with.contains("refonds l'UI"));
    }

    #[test]
    fn take_goal_done_detects_and_strips_marker() {
        let mut s = AppState::new("gpt-5", false);
        // Pas de marqueur → non atteint.
        s.blocks.push(Block::Assistant {
            text: "j'ai commencé".into(),
            streaming: false,
        });
        assert!(!take_goal_done(&mut s));
        // Marqueur présent → atteint + strippé de l'affichage.
        s.blocks.push(Block::Assistant {
            text: format!("c'est terminé\n{GOAL_DONE_MARKER}"),
            streaming: false,
        });
        assert!(take_goal_done(&mut s));
        assert!(
            matches!(s.blocks.last(), Some(Block::Assistant { text, .. })
                if text == "c'est terminé" && !text.contains(GOAL_DONE_MARKER)),
            "marqueur strippé du dernier bloc",
        );
    }

    #[test]
    fn take_goal_done_requires_marker_as_last_line() {
        let mut s = AppState::new("gpt-5", false);
        s.blocks.push(Block::Assistant {
            text: format!("texte {GOAL_DONE_MARKER} au milieu"),
            streaming: false,
        });
        assert!(!take_goal_done(&mut s));

        s.blocks.push(Block::Assistant {
            text: format!("terminé\n{GOAL_DONE_MARKER}\n\n"),
            streaming: false,
        });
        assert!(take_goal_done(&mut s));
    }

    #[test]
    fn session_path_from_arg_rejects_path_traversal() {
        let sessions = Path::new("/tmp/pyxis-sessions");
        assert_eq!(
            session_path_from_arg(sessions, "123.jsonl").unwrap(),
            sessions.join("123.jsonl")
        );
        assert!(session_path_from_arg(sessions, "../123.jsonl").is_none());
        assert!(session_path_from_arg(sessions, "/tmp/123.jsonl").is_none());
        assert!(session_path_from_arg(sessions, "nested/123.jsonl").is_none());
        assert!(session_path_from_arg(sessions, "123.txt").is_none());
    }

    #[test]
    fn scrub_encrypted_reasoning_removes_only_replay_blocks() {
        let mut messages = vec![Message::assistant(vec![
            ContentBlock::Text { text: "ok".into() },
            ContentBlock::EncryptedReasoning {
                id: "rs_1".into(),
                encrypted_content: "ENC".into(),
            },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            },
        ])];
        assert_eq!(count_encrypted_reasoning(&messages), 1);
        assert_eq!(scrub_encrypted_reasoning(&mut messages), 1);
        assert_eq!(count_encrypted_reasoning(&messages), 0);
        assert!(
            messages[0]
                .content
                .iter()
                .all(|b| !matches!(b, ContentBlock::EncryptedReasoning { .. }))
        );
        assert_eq!(messages[0].content.len(), 2);
    }
}
