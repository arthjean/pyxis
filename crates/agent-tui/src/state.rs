//! État de rendu côté client (US-019). `AppState` consomme les `AgentEvent` du
//! cœur (jamais d'ANSI) et les range en `Block`s typés ; le rendu (`render.rs`)
//! décide seul de la présentation. La gestion clavier renvoie une `InputAction`
//! que la boucle agent-cli interprète (soumission, permission, quit, scroll).

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use agent_core::AgentEvent;
use agent_core::message::{ContentBlock, Message, Role, ToolCallId, ToolErrorKind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

/// Un élément du transcript. Le rendu choisit poids/teinte ; aucune couleur ici.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    /// Tour utilisateur.
    User(String),
    /// Tour assistant (texte streamé). `streaming` = curseur live actif.
    Assistant { text: String, streaming: bool },
    /// Raisonnement du modèle (rendu en sourdine).
    Reasoning(String),
    /// Un outil va s'exécuter. L'`input` brut est CONSERVÉ (US-033) : le rendu en
    /// dérive le label `Verb(cible)` et, à terme, le diff (EP-011) ; `id` apparie
    /// l'appel à son résultat.
    ToolCall {
        id: ToolCallId,
        name: String,
        input: serde_json::Value,
        input_hash: u64,
    },
    /// Résultat d'un outil (taint + erreur portés pour le rendu). `call_id` pointe
    /// vers le `ToolCall` correspondant (US-033) pour le résumé `⎿`.
    ToolResult {
        call_id: ToolCallId,
        content: String,
        untrusted: bool,
        is_error: bool,
        error_kind: Option<ToolErrorKind>,
    },
    /// Information système discrète (compaction, budget…).
    Notice(String),
    /// Erreur remontée par le cœur.
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Thinking,
}

/// Commandes slash : (nom, description, prend-un-argument). Source unique pour le
/// menu de complétion (rendu) ET l'exécution (boucle agent-cli). `takes_arg` =
/// la commande ouvre un sous-menu / attend un argument (Entrée complète au lieu
/// d'exécuter). Ajouter = une ligne ici + une branche dans le dispatch.
pub const COMMANDS: &[(&str, &str, bool)] = &[
    ("/help", "Show available commands", false),
    ("/models", "Choose the active model", true),
    (
        "/permissions",
        "Choose when Pyxis asks for confirmation",
        true,
    ),
    ("/skills", "Insert a skill into the message", true),
    ("/goal", "Set a goal and work until it is done", true),
    ("/providers", "Configure the authentication provider", true),
    ("/mcp", "Inspect MCP servers", true),
    ("/resume", "Resume a past conversation", true),
    ("/new", "Start a new session and clear context", false),
    ("/clear", "Clear context and start fresh", false),
    ("/quit", "Quit Pyxis", false),
];

/// Niveau 1 de `/providers` : (id, libellé, actif). Seul l'abonnement est
/// disponible pour l'instant ; la clé API est annoncée mais inactive.
pub const AUTH_KINDS: &[(&str, &str, bool)] = &[
    ("subscription", "Use a subscription", true),
    ("apikey", "Use an API key", false),
];

/// Niveau 2 de `/providers subscription` : (id, libellé, actif). Seul Codex
/// (abonnement ChatGPT) est branché ; les autres sont annoncés.
pub const SUB_PROVIDERS: &[(&str, &str, bool)] = &[
    ("codex", "ChatGPT Plus/Pro (Codex Subscription)", true),
    ("anthropic", "Anthropic (Claude Pro/Max)", false),
];

/// Modèles disponibles : (slug, tag provider). Sous-menu de `/models`. Le premier
/// est le défaut (cf. `agent_provider::DEFAULT_MODEL`). Liste VOLATILE : le
/// backend Codex retire/ajoute des slugs (cf. mémoire abonnement ChatGPT).
pub const MODELS: &[(&str, &str)] = &[
    ("gpt-5.5", "[openai-codex]"),
    ("gpt-5.4", "[openai-codex]"),
    ("gpt-5.4-mini", "[openai-codex]"),
    ("gpt-5.3-codex-spark", "[openai-codex]"),
];

pub const DEFAULT_PERMISSION_MODE_ID: &str = "ask";
pub const QUIT_SHORTCUT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionModeMeta {
    pub id: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

pub const PERMISSION_MODES: &[PermissionModeMeta] = &[
    PermissionModeMeta {
        id: "ask",
        label: "Ask for approval",
        hint: "ask before sensitive actions",
    },
    PermissionModeMeta {
        id: "accept-edits",
        label: "Auto-approve edits",
        hint: "auto-approve write/edit, ask for sensitive actions",
    },
    PermissionModeMeta {
        id: "auto",
        label: "Approve for me",
        hint: "do not interrupt except after recent taint",
    },
    PermissionModeMeta {
        id: "full-access",
        label: "Full Access",
        hint: "bypass confirmations, sandbox unchanged",
    },
    PermissionModeMeta {
        id: "read-only",
        label: "Read Only",
        hint: "strict read-only mode",
    },
];

pub fn permission_mode_meta(id: &str) -> Option<&'static PermissionModeMeta> {
    PERMISSION_MODES.iter().find(|mode| mode.id == id)
}

pub fn permission_mode_label(id: &str) -> &'static str {
    permission_mode_meta(id)
        .map(|mode| mode.label)
        .unwrap_or("Ask for approval")
}

/// Le texte est-il une vraie commande Pyxis ? (1er mot ∈ COMMANDS). Un message
/// qui commence par un `/<skill>` n'en est PAS une → il part à l'agent.
fn is_command(text: &str) -> bool {
    let first = text.split(' ').next().unwrap_or("");
    COMMANDS.iter().any(|(name, _, _)| *name == first)
}

/// La commande `name` attend-elle un argument / un sous-menu ?
fn command_takes_arg(name: &str) -> bool {
    COMMANDS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, _, takes)| *takes)
        .unwrap_or(false)
}

/// Un item de menu de complétion (source unifiée : commandes, modèles, sessions,
/// providers). `id` = valeur passée à l'action ; `label`/`hint` = affichage ;
/// `enabled` = sélectionnable (les items « bientôt » sont grisés).
#[derive(Debug, Clone)]
pub struct MenuItem {
    pub id: String,
    pub label: String,
    pub hint: String,
    pub enabled: bool,
}

impl MenuItem {
    fn new(id: &str, label: &str, hint: &str, enabled: bool) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            hint: hint.to_string(),
            enabled,
        }
    }
}

/// Quel sous-menu la saisie courante ouvre-t-elle ? (fil d'Ariane dans l'input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Menu {
    None,
    Commands,
    Models,
    Resume,
    Skills,
    Files,
    Permissions,
    ProviderAuth,
    ProviderList,
    /// Niveau 3 : actions sur un provider (connect/disconnect).
    ProviderActions,
    /// `/mcp ` : liste des serveurs MCP (badge de statut).
    McpList,
    /// `/mcp <serveur> ` : actions sur un serveur (connect/disconnect/tools).
    McpActions,
}

/// Entrée du sous-menu `/resume` (remplie par agent-cli depuis le disque).
#[derive(Debug, Clone)]
pub struct SessionMeta {
    /// Identifiant résolu côté CLI (nom de fichier `<id>.jsonl`).
    pub id: String,
    /// Libellé affiché : résumé de la conversation (1er message).
    pub label: String,
    /// Indice secondaire affiché en sourdine (ex. « 12 msgs · il y a 2 h »).
    pub hint: String,
}

/// Statut de connexion d'un serveur MCP (sous-menu `/mcp`). Calque l'enum
/// `agent_mcp::McpServer` côté affichage — agent-cli fait le mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpStatus {
    Disconnected,
    Connecting,
    Connected,
    Failed,
}

/// Entrée du sous-menu `/mcp` (remplie par agent-cli depuis le registre MCP).
#[derive(Debug, Clone)]
pub struct McpServerMeta {
    pub name: String,
    pub status: McpStatus,
    pub source: String,
    pub needs_trust: bool,
    /// Nombre d'outils exposés (significatif seulement si `Connected`).
    pub tool_count: usize,
}

/// Reconstruit le transcript affichable depuis des messages canoniques (resume
/// d'une session). Inverse approximatif d'`AppState::apply` : System ignoré,
/// thinking → reasoning, tool_use → tool call, tool_result → résultat.
pub fn blocks_from_messages(messages: &[Message]) -> Vec<Block> {
    let mut blocks = Vec::new();
    for m in messages {
        match m.role {
            Role::System => {}
            Role::User => {
                let t = m.text();
                if !t.is_empty() {
                    blocks.push(Block::User(t));
                }
            }
            Role::Assistant => {
                for b in &m.content {
                    if let ContentBlock::Thinking { text } = b {
                        blocks.push(Block::Reasoning(text.clone()));
                    }
                }
                let text = m.text();
                if !text.is_empty() {
                    blocks.push(Block::Assistant {
                        text,
                        streaming: false,
                    });
                }
                for b in &m.content {
                    if let ContentBlock::ToolUse { id, name, input } = b {
                        blocks.push(Block::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                            input_hash: crate::cache::value_hash(input),
                        });
                    }
                }
            }
            Role::Tool => {
                for b in &m.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        untrusted,
                        is_error,
                        error_kind,
                    } = b
                    {
                        blocks.push(Block::ToolResult {
                            call_id: tool_use_id.clone(),
                            content: content.clone(),
                            untrusted: *untrusted,
                            is_error: *is_error,
                            error_kind: *error_kind,
                        });
                    }
                }
            }
        }
    }
    blocks
}

/// Extrait l'historique des prompts (messages utilisateur, ancien → récent) d'une
/// session reprise, pour la navigation aux flèches.
pub fn prompts_from_messages(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(Message::text)
        .filter(|t| !t.trim().is_empty())
        .collect()
}

/// Demande de confirmation présentée à l'utilisateur (générique : la boucle
/// agent-cli la construit depuis la `PermissionRequest` d'`agent-tools`, en
/// pré-rendant l'aperçu via `diff` : vrai diff pour `edit`/`write`, lignes de
/// contexte pour bash/inconnu, PARTAGÉ avec le diff inline du transcript (US-039).
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionPrompt {
    pub title: String,
    pub reason: String,
    pub preview: crate::diff::Diff,
    pub call_id: Option<ToolCallId>,
    pub mode: Option<String>,
    pub taint_forced: bool,
}

impl PermissionPrompt {
    pub fn new(
        title: impl Into<String>,
        reason: impl Into<String>,
        preview: crate::diff::Diff,
    ) -> Self {
        Self {
            title: title.into(),
            reason: reason.into(),
            preview,
            call_id: None,
            mode: None,
            taint_forced: false,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub blocks: Vec<Block>,
    pub input: String,
    /// Position du curseur dans l'input, en offset byte UTF-8 valide.
    /// Les mouvements/suppressions suivent les graphèmes ; le rendu convertit cet
    /// offset en largeur terminale via `unicode-width`.
    pub cursor: usize,
    pub status: Status,
    pub pending: Option<PermissionPrompt>,
    pub truecolor: bool,
    /// Décalage de scroll vers le HAUT (0 = collé en bas, suit le live).
    pub scroll: usize,
    /// Borne max du scroll, recalculée à chaque frame par le rendu (lignes APRÈS
    /// wrap − hauteur visible). Cache de feedback rendu→entrée : permet de clamper
    /// le scroll sans dupliquer le calcul de wrap hors de `render`.
    pub scroll_max: Cell<usize>,
    /// Cache des lignes stylées par bloc (US-041) : ne reconstruire que le bloc en
    /// stream, servir les autres depuis le cache. Interior mutability (même patron
    /// que `scroll_max`) pour que `render` reste pur (signature `&AppState`).
    pub(crate) render_cache: RefCell<crate::cache::RenderCache>,
    pub model: String,
    /// Nom du workspace (dossier courant) affiché dans la status line ; vide = masqué.
    pub workspace: String,
    /// Fraction de contexte consommée (0–100). `None` = inconnue → segment masqué.
    pub context_pct: Option<u8>,
    /// Effort de raisonnement affiché avec le modèle dans le footer.
    pub reasoning_effort: Option<String>,
    /// Mode de permission affiché dans le footer et le sous-menu `/permissions`.
    permission_mode: String,
    /// Index sélectionné dans le menu de commandes slash (0 = première ligne).
    pub completion_index: usize,
    /// Sessions reprenables (sous-menu `/resume`), remplies par agent-cli.
    pub sessions: Vec<SessionMeta>,
    /// Skills disponibles (`~/.agents/skills`), sous-menu `/skills`. Lus avant le
    /// sandbox (dossier hors workspace) et injectés par agent-cli.
    pub skills: Vec<String>,
    /// Fichiers mentionnables via `@`, bornés et fournis par agent-cli.
    pub files: Vec<String>,
    /// Connecté au fournisseur actif (badge status line + sous-menu providers).
    pub provider_connected: bool,
    /// Serveurs MCP connus + statut (sous-menu `/mcp`), remplis par agent-cli.
    pub mcp_servers: Vec<McpServerMeta>,
    /// Historique des prompts soumis (ancien → récent), navigable aux flèches.
    pub history: Vec<String>,
    /// Position dans l'historique : `None` = brouillon courant, `Some(i)` = sur
    /// `history[i]`. Brouillon sauvegardé dans `draft` au premier Haut.
    history_pos: Option<usize>,
    draft: String,
    pub should_quit: bool,
    shutdown_in_progress: bool,
    quit_shortcut_expires_at: Option<Instant>,
    // ── Progression vivante (EP-013) ────────────────────────────────────────────
    /// Tick d'animation du spinner, avancé par la boucle (~10 fps) tant qu'un tour
    /// est actif. Le rendu choisit la frame depuis ce compteur (reste pur).
    pub spinner_tick: usize,
    /// Durée écoulée du tour en cours (`None` hors tour) ; alimentée par la boucle
    /// (qui possède l'horloge) — `render` ne lit jamais l'heure.
    pub turn_elapsed: Option<Duration>,
    /// Caractères cumulés (texte + raisonnement) du tour en cours → estimation de
    /// tokens (/4). Sur une boucle `/goal`, cumule l'ensemble des relances (vue coût
    /// total) : remis à zéro seulement au front montant de `running` (`begin_turn`).
    pub turn_chars: usize,
    /// Reduced-motion (`NO_COLOR` / `PYXIS_REDUCED_MOTION`) : spinner dégradé en point pulsé.
    pub reduced_motion: bool,
    /// Nouveaux blocs arrivés pendant que l'utilisateur a remonté le transcript
    /// (pill « revenir en bas », US-046). Remis à 0 dès le retour au bas.
    pub unseen: usize,
    /// Overlay transcript complet, ouvert par Ctrl+T. Son scroll est séparé du
    /// scroll du fil principal pour revenir exactement où l'utilisateur était.
    transcript_overlay_open: bool,
    transcript_overlay_scroll: usize,
    transcript_overlay_scroll_max: Cell<usize>,
    transcript_overlay_page_height: Cell<usize>,
    /// Début du stream live courant : index de bloc et compteur de caractères.
    /// Utilisé pour retirer les deltas abandonnés quand le core retry/recover.
    stream_start: Option<(usize, usize)>,
}

/// Action déduite d'une touche, interprétée par la boucle agent-cli.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    None,
    Submit(String),
    /// Commande slash à exécuter (ligne complète, args inclus : `/model gpt-5.5`).
    Command(String),
    Interrupt,
    Quit,
    Permission(bool),
    ScrollUp,
    ScrollDown,
}

fn is_ctrl_key(key: &KeyEvent, expected: char) -> bool {
    matches!(
        key.code,
        KeyCode::Char(c)
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && c.eq_ignore_ascii_case(&expected)
    )
}

fn is_plain_char_key(key: &KeyEvent, expected: char) -> bool {
    matches!(
        key.code,
        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
                && c.eq_ignore_ascii_case(&expected)
    )
}

impl AppState {
    pub fn new(model: impl Into<String>, truecolor: bool) -> Self {
        Self {
            blocks: Vec::new(),
            input: String::new(),
            cursor: 0,
            status: Status::Idle,
            pending: None,
            truecolor,
            scroll: 0,
            scroll_max: Cell::new(0),
            render_cache: RefCell::new(crate::cache::RenderCache::default()),
            model: model.into(),
            workspace: String::new(),
            context_pct: None,
            reasoning_effort: None,
            permission_mode: DEFAULT_PERMISSION_MODE_ID.to_string(),
            completion_index: 0,
            sessions: Vec::new(),
            skills: Vec::new(),
            files: Vec::new(),
            provider_connected: false,
            mcp_servers: Vec::new(),
            history: Vec::new(),
            history_pos: None,
            draft: String::new(),
            should_quit: false,
            shutdown_in_progress: false,
            quit_shortcut_expires_at: None,
            spinner_tick: 0,
            turn_elapsed: None,
            turn_chars: 0,
            reduced_motion: false,
            unseen: 0,
            transcript_overlay_open: false,
            transcript_overlay_scroll: 0,
            transcript_overlay_scroll_max: Cell::new(0),
            transcript_overlay_page_height: Cell::new(10),
            stream_start: None,
        }
    }

    // ── Édition de l'input avec curseur positionnable ──────────────────────────

    fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.input.len());
        while self.cursor > 0 && !self.input.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    fn prev_grapheme_boundary(&self) -> Option<usize> {
        self.input[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(idx, _)| idx)
    }

    fn next_grapheme_boundary(&self) -> Option<usize> {
        self.input[self.cursor..]
            .grapheme_indices(true)
            .nth(1)
            .map(|(idx, _)| self.cursor + idx)
            .or_else(|| (self.cursor < self.input.len()).then_some(self.input.len()))
    }

    /// Remplace l'input et place le curseur en fin (recall, complétion, insertion).
    pub fn set_input(&mut self, value: String) {
        self.cursor = value.len();
        self.input = value;
    }

    pub fn permission_mode_id(&self) -> &str {
        &self.permission_mode
    }

    pub fn permission_mode_label(&self) -> &'static str {
        permission_mode_label(&self.permission_mode)
    }

    pub fn set_permission_mode(&mut self, id: impl Into<String>) {
        let id = id.into();
        self.permission_mode = if permission_mode_meta(&id).is_some() {
            id
        } else {
            DEFAULT_PERMISSION_MODE_ID.to_string()
        };
    }

    pub fn quit_shortcut_hint_visible(&self) -> bool {
        self.quit_shortcut_expires_at
            .is_some_and(|expires_at| Instant::now() < expires_at)
    }

    pub fn quit_shortcut_remaining(&self) -> Option<Duration> {
        self.quit_shortcut_expires_at
            .and_then(|expires_at| expires_at.checked_duration_since(Instant::now()))
    }

    pub fn clear_quit_shortcut_hint(&mut self) {
        self.quit_shortcut_expires_at = None;
    }

    pub fn shutdown_in_progress(&self) -> bool {
        self.shutdown_in_progress
    }

    pub fn show_shutdown_in_progress(&mut self) {
        self.shutdown_in_progress = true;
        self.pending = None;
        self.status = Status::Idle;
        self.completion_index = 0;
        self.clear_quit_shortcut_hint();
    }

    fn arm_quit_shortcut(&mut self) {
        self.quit_shortcut_expires_at = Instant::now()
            .checked_add(QUIT_SHORTCUT_TIMEOUT)
            .or_else(|| Some(Instant::now()));
    }

    fn quit_shortcut_active(&self) -> bool {
        self.quit_shortcut_hint_visible()
    }

    fn on_ctrl_c(&mut self) -> InputAction {
        if self.quit_shortcut_active() {
            self.clear_quit_shortcut_hint();
            self.should_quit = true;
            return InputAction::Quit;
        }

        self.arm_quit_shortcut();
        if self.status == Status::Thinking {
            InputAction::Interrupt
        } else {
            InputAction::None
        }
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    /// Insère un char à la position du curseur.
    pub fn insert_char(&mut self, c: char) {
        self.clamp_cursor();
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Insère une chaîne à la position du curseur (le curseur la suit).
    pub fn insert_str(&mut self, s: &str) {
        self.clamp_cursor();
        self.input.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Supprime le char AVANT le curseur (Backspace).
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.clamp_cursor();
        if let Some(start) = self.prev_grapheme_boundary() {
            self.input.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    /// Supprime le char SOUS le curseur (Delete).
    pub fn delete(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.input.len() {
            return;
        }
        let end = self.next_grapheme_boundary().unwrap_or(self.input.len());
        self.input.replace_range(self.cursor..end, "");
    }

    fn move_left(&mut self) {
        self.clamp_cursor();
        if let Some(prev) = self.prev_grapheme_boundary() {
            self.cursor = prev;
        }
    }
    fn move_right(&mut self) {
        self.clamp_cursor();
        if let Some(next) = self.next_grapheme_boundary() {
            self.cursor = next;
        }
    }
    fn move_home(&mut self) {
        self.cursor = 0;
    }
    fn move_end(&mut self) {
        self.cursor = self.input.len();
    }

    fn delete_prev_word(&mut self) {
        self.clamp_cursor();
        while self.cursor > 0 {
            let Some(prev) = self.prev_grapheme_boundary() else {
                break;
            };
            if !self.input[prev..self.cursor].trim().is_empty() {
                break;
            }
            self.input.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
        while self.cursor > 0 {
            let Some(prev) = self.prev_grapheme_boundary() else {
                break;
            };
            if self.input[prev..self.cursor].trim().is_empty() {
                break;
            }
            self.input.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
    }

    /// Range un `AgentEvent` du cœur dans le transcript.
    pub fn apply(&mut self, ev: &AgentEvent) {
        let before = self.blocks.len();
        match ev {
            AgentEvent::StreamReset => self.reset_streaming(),
            AgentEvent::Text(t) => {
                self.begin_streaming();
                self.status = Status::Thinking;
                self.turn_chars += t.chars().count();
                match self.blocks.last_mut() {
                    Some(Block::Assistant {
                        text,
                        streaming: true,
                    }) => text.push_str(t),
                    _ => self.blocks.push(Block::Assistant {
                        text: t.clone(),
                        streaming: true,
                    }),
                }
            }
            AgentEvent::Reasoning(t) => {
                self.begin_streaming();
                self.status = Status::Thinking;
                self.turn_chars += t.chars().count();
                match self.blocks.last_mut() {
                    Some(Block::Reasoning(r)) => r.push_str(t),
                    _ => self.blocks.push(Block::Reasoning(t.clone())),
                }
            }
            AgentEvent::ToolCall(view) => {
                self.finalize_streaming();
                self.blocks.push(Block::ToolCall {
                    id: view.id.clone(),
                    name: view.name.clone(),
                    input: view.input.clone(),
                    input_hash: crate::cache::value_hash(&view.input),
                });
            }
            AgentEvent::ToolResult(view) => {
                // Symétrie défensive avec ToolCall : si un résultat orphelin arrivait
                // sans appel préalable, un Assistant{streaming} resté ouvert ne doit pas
                // garder un curseur live fantôme.
                self.finalize_streaming();
                self.blocks.push(Block::ToolResult {
                    call_id: view.id.clone(),
                    content: view.content.clone(),
                    untrusted: view.untrusted,
                    is_error: view.is_error,
                    error_kind: view.error_kind,
                });
            }
            AgentEvent::Compacted(_) => self.blocks.push(Block::Notice("context compacted".into())),
            AgentEvent::PermissionAsk(req) => self
                .blocks
                .push(Block::Notice(format!("permission: {}", req.tool))),
            AgentEvent::EndTurn => {
                self.finalize_streaming();
                self.status = Status::Idle;
            }
            AgentEvent::Interrupted => {
                self.finalize_streaming();
                self.pending = None;
                self.blocks.push(Block::Notice("interrupted".into()));
                self.status = Status::Idle;
            }
            AgentEvent::Exhausted(reason) => {
                self.finalize_streaming();
                self.blocks
                    .push(Block::Notice(format!("stopped: {reason:?}")));
                self.status = Status::Idle;
            }
            AgentEvent::Error(e) => {
                self.finalize_streaming();
                self.blocks.push(Block::Error(e.to_string()));
                self.status = Status::Idle;
            }
        }
        // Pill « nouveau message » (US-046) : si l'utilisateur a remonté le
        // transcript, signaler le contenu apparu hors de sa vue.
        if self.scroll > 0 {
            if self.blocks.len() > before {
                self.unseen += self.blocks.len() - before;
            } else if matches!(ev, AgentEvent::Text(_) | AgentEvent::Reasoning(_)) {
                // Stream qui APPEND au dernier bloc (pas de nouveau bloc) : signaler au
                // moins « du contenu est arrivé » sans gonfler le compteur par token.
                self.unseen = self.unseen.max(1);
            }
        }
    }

    /// Pousse le tour utilisateur (appelé à la soumission) et l'enregistre dans
    /// l'historique navigable (dédup consécutive, façon `ignoredups`).
    pub fn push_user(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self.history.last().map(String::as_str) != Some(text.as_str()) {
            self.history.push(text.clone());
        }
        self.history_pos = None;
        self.draft.clear();
        self.blocks.push(Block::User(text));
        self.status = Status::Thinking;
        self.scroll = 0;
        self.unseen = 0;
    }

    /// Remplace l'historique navigable (resume d'une session) et réinitialise la
    /// navigation.
    pub fn load_history(&mut self, prompts: Vec<String>) {
        self.history = prompts;
        self.history_pos = None;
        self.draft.clear();
    }

    /// Flèche Haut : remonte vers un prompt plus ancien. Sauvegarde le brouillon
    /// au premier appui ; se bloque sur le plus ancien (pas de wrap).
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.draft = std::mem::take(&mut self.input);
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_pos = Some(pos);
        let v = self.history[pos].clone();
        self.set_input(v);
        self.completion_index = 0;
    }

    /// Flèche Bas : redescend vers un prompt plus récent ; au-delà du plus récent,
    /// restaure le brouillon.
    pub fn history_next(&mut self) {
        match self.history_pos {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.history_pos = Some(i + 1);
                let v = self.history[i + 1].clone();
                self.set_input(v);
                self.completion_index = 0;
            }
            Some(_) => {
                self.history_pos = None;
                let d = std::mem::take(&mut self.draft);
                self.set_input(d);
                self.completion_index = 0;
            }
        }
    }

    fn finalize_streaming(&mut self) {
        if let Some(Block::Assistant { streaming, .. }) = self.blocks.last_mut() {
            *streaming = false;
        }
        self.stream_start = None;
    }

    fn begin_streaming(&mut self) {
        if self.stream_start.is_none() {
            self.stream_start = Some((self.blocks.len(), self.turn_chars));
        }
    }

    fn reset_streaming(&mut self) {
        if let Some((block_start, chars_start)) = self.stream_start.take() {
            self.blocks.truncate(block_start);
            self.turn_chars = chars_start;
        }
        self.status = Status::Thinking;
    }

    /// Remonte dans le transcript de `n` lignes, clampé à la borne calculée au
    /// dernier rendu (`scroll_max`) — pas de sur-scroll au-delà du début.
    pub fn scroll_up(&mut self, n: u16) {
        // Quitter le bas repart d'un compteur vierge : tout `unseen` résiduel (ex. un
        // bloc poussé pendant qu'on était déjà collé en bas) est écarté ; on ne
        // comptera que le contenu arrivant APRÈS ce scroll (US-046).
        if self.scroll == 0 {
            self.unseen = 0;
        }
        self.scroll = self
            .scroll
            .saturating_add(n as usize)
            .min(self.scroll_max.get());
    }

    /// Redescend de `n` lignes (0 = collé en bas, suit le live).
    pub fn scroll_down(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n as usize);
        // Retour au bas → l'auto-follow reprend, plus de « nouveaux messages » (US-046).
        if self.scroll == 0 {
            self.unseen = 0;
        }
    }

    pub fn transcript_overlay_open(&self) -> bool {
        self.transcript_overlay_open
    }

    pub fn transcript_overlay_scroll(&self) -> usize {
        self.transcript_overlay_scroll
    }

    pub fn open_transcript_overlay(&mut self) {
        self.transcript_overlay_open = true;
        self.transcript_overlay_scroll = 0;
    }

    pub fn close_transcript_overlay(&mut self) {
        self.transcript_overlay_open = false;
    }

    pub fn set_transcript_overlay_metrics(&self, max_scroll: usize, page_height: u16) {
        self.transcript_overlay_scroll_max.set(max_scroll);
        self.transcript_overlay_page_height
            .set((page_height as usize).max(1));
    }

    fn transcript_overlay_scroll_up(&mut self, n: usize) {
        self.transcript_overlay_scroll = self
            .transcript_overlay_scroll
            .saturating_add(n)
            .min(self.transcript_overlay_scroll_max.get());
    }

    fn transcript_overlay_scroll_down(&mut self, n: usize) {
        self.transcript_overlay_scroll = self.transcript_overlay_scroll.saturating_sub(n);
    }

    fn transcript_overlay_page_height(&self) -> usize {
        self.transcript_overlay_page_height.get().max(1)
    }

    fn jump_transcript_overlay_top(&mut self) {
        self.transcript_overlay_scroll = self.transcript_overlay_scroll_max.get();
    }

    fn jump_transcript_overlay_bottom(&mut self) {
        self.transcript_overlay_scroll = 0;
    }

    /// Nombre de blocs reconstruits au dernier rendu (instrumentation US-041) : 0 =
    /// tout servi depuis le cache. Exposé pour les tests de performance du cache.
    pub fn render_rebuilds(&self) -> usize {
        self.render_cache.borrow().rebuilds()
    }

    /// Démarre le suivi de progression d'un tour (front montant de `running` côté
    /// boucle, US-044/045) : remet à zéro spinner, durée et compteur de tokens.
    pub fn begin_turn(&mut self) {
        self.spinner_tick = 0;
        self.turn_elapsed = None;
        self.turn_chars = 0;
    }

    /// Avance l'animation et met à jour la durée écoulée (appelé par le tick de la
    /// boucle tant qu'un tour est actif, US-044/045). `render` reste pur : il ne lit
    /// jamais l'horloge, il consomme ces valeurs.
    pub fn tick_progress(&mut self, elapsed: Duration) {
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
        self.turn_elapsed = Some(elapsed);
    }

    /// Fin de tour (front descendant de `running`) : les indicateurs disparaissent
    /// proprement, sans compteur qui continue (US-045).
    pub fn end_turn(&mut self) {
        self.turn_elapsed = None;
    }

    /// Quel sous-menu la saisie ouvre-t-elle ? (fil d'Ariane dans l'input :
    /// `/providers subscription …` = niveau 2, `/providers …` = niveau 1, etc.)
    fn menu_kind(&self) -> Menu {
        let i = self.input.as_str();
        if let Some(rest) = i.strip_prefix("/providers ") {
            if let Some(rest2) = rest.strip_prefix("subscription ") {
                // « <provider> » suivi d'un espace → niveau 3 (actions du provider).
                let prov = rest2.split(' ').next().unwrap_or("");
                if !prov.is_empty()
                    && rest2.len() > prov.len()
                    && SUB_PROVIDERS.iter().any(|(id, _, _)| *id == prov)
                {
                    Menu::ProviderActions
                } else {
                    Menu::ProviderList
                }
            } else {
                Menu::ProviderAuth
            }
        } else if i.strip_prefix("/mcp ").is_some() {
            // McpActions dès qu'un serveur connu est entièrement saisi (suivi d'un
            // espace) ; sinon on filtre encore la liste. `active_mcp_server` gère
            // les noms contenant des espaces.
            if self.active_mcp_server().is_empty() {
                Menu::McpList
            } else {
                Menu::McpActions
            }
        } else if i.starts_with("/resume ") {
            Menu::Resume
        } else if i.starts_with("/models ") {
            Menu::Models
        } else if i.starts_with("/permissions ") {
            Menu::Permissions
        } else if i.starts_with("/skills ") {
            Menu::Skills
        } else if self.active_file_query().is_some() {
            Menu::Files
        } else if i.starts_with('/') && !i.contains(' ') {
            Menu::Commands
        } else {
            Menu::None
        }
    }

    /// Items du menu de complétion selon le sous-menu actif. Source unifiée :
    /// commandes, modèles, sessions (dynamiques), niveaux de `/providers`.
    pub fn menu_items(&self) -> Vec<MenuItem> {
        match self.menu_kind() {
            Menu::None => Vec::new(),
            Menu::Commands => COMMANDS
                .iter()
                .filter(|(name, _, _)| name.starts_with(self.input.as_str()))
                .map(|(name, desc, _)| MenuItem::new(name, name, desc, true))
                .collect(),
            Menu::Models => {
                let q = self.input.strip_prefix("/models ").unwrap_or("");
                let mut items = MODELS
                    .iter()
                    .filter(|(slug, _)| slug.starts_with(q))
                    .map(|(slug, tag)| MenuItem::new(slug, slug, tag, true))
                    .collect::<Vec<_>>();
                if !q.trim().is_empty() && !MODELS.iter().any(|(slug, _)| *slug == q) {
                    items.push(MenuItem::new(q, q, "custom", true));
                }
                items
            }
            Menu::Permissions => {
                let q = self.input.strip_prefix("/permissions ").unwrap_or("");
                PERMISSION_MODES
                    .iter()
                    .filter(|mode| q.is_empty() || mode.id.starts_with(q) || mode.label.contains(q))
                    .map(|mode| {
                        let label = if mode.id == self.permission_mode {
                            format!("{} (current)", mode.label)
                        } else {
                            mode.label.to_string()
                        };
                        MenuItem::new(mode.id, &label, mode.hint, true)
                    })
                    .collect()
            }
            Menu::Resume => self
                .sessions
                .iter()
                .filter(|s| {
                    let q = self.input.strip_prefix("/resume ").unwrap_or("");
                    q.is_empty() || s.id.starts_with(q) || s.label.contains(q)
                })
                .map(|s| MenuItem {
                    id: s.id.clone(),
                    label: s.label.clone(),
                    hint: s.hint.clone(),
                    enabled: true,
                })
                .collect(),
            Menu::Skills => {
                let q = self.input.strip_prefix("/skills ").unwrap_or("");
                self.skills
                    .iter()
                    .filter(|name| name.contains(q))
                    .map(|name| MenuItem::new(name, name, "", true))
                    .collect()
            }
            Menu::Files => {
                let Some((_, q)) = self.active_file_query() else {
                    return Vec::new();
                };
                let mut items = self
                    .files
                    .iter()
                    .filter(|path| q.is_empty() || path.contains(q))
                    .take(20)
                    .map(|path| MenuItem::new(path, path, "file", true))
                    .collect::<Vec<_>>();
                if items.is_empty() {
                    items.push(MenuItem::new("", "No files", "", false));
                }
                items
            }
            Menu::ProviderAuth => {
                let q = self.input.strip_prefix("/providers ").unwrap_or("");
                AUTH_KINDS
                    .iter()
                    .filter(|(id, _, _)| id.starts_with(q))
                    .map(|(id, label, en)| {
                        MenuItem::new(id, label, if *en { "" } else { "coming soon" }, *en)
                    })
                    .collect()
            }
            Menu::ProviderList => {
                let q = self
                    .input
                    .strip_prefix("/providers subscription ")
                    .unwrap_or("");
                SUB_PROVIDERS
                    .iter()
                    .filter(|(id, _, _)| id.starts_with(q))
                    .map(|(id, label, en)| {
                        let hint = if *id == "codex" {
                            if self.provider_connected {
                                "connected"
                            } else {
                                "not connected"
                            }
                        } else if *en {
                            ""
                        } else {
                            "coming soon"
                        };
                        MenuItem::new(id, label, hint, *en)
                    })
                    .collect()
            }
            Menu::ProviderActions => {
                // Connect actif seulement si déconnecté ; Disconnect l'inverse.
                let c = self.provider_connected;
                vec![
                    MenuItem::new(
                        "connect",
                        "Connect",
                        if c { "already connected" } else { "" },
                        !c,
                    ),
                    MenuItem::new(
                        "disconnect",
                        "Disconnect",
                        if c { "" } else { "already disconnected" },
                        c,
                    ),
                ]
            }
            Menu::McpList => {
                let q = self.input.strip_prefix("/mcp ").unwrap_or("");
                if self.mcp_servers.is_empty() {
                    return vec![MenuItem::new(
                        "",
                        "No MCP servers",
                        "add .mcp.json to the workspace",
                        false,
                    )];
                }
                self.mcp_servers
                    .iter()
                    .filter(|m| m.name.starts_with(q))
                    .map(|m| {
                        let hint = match m.status {
                            McpStatus::Connected => {
                                format!("{} · connected · {} tools", m.source, m.tool_count)
                            }
                            McpStatus::Connecting => format!("{} · connecting...", m.source),
                            McpStatus::Failed => format!("{} · failed", m.source),
                            McpStatus::Disconnected if m.needs_trust => {
                                format!("{} · trust required", m.source)
                            }
                            McpStatus::Disconnected => format!("{} · not connected", m.source),
                        };
                        MenuItem::new(&m.name, &m.name, &hint, true)
                    })
                    .collect()
            }
            Menu::McpActions => {
                let srv = self.active_mcp_server();
                let status = self
                    .mcp_servers
                    .iter()
                    .find(|m| m.name == srv)
                    .map(|m| (m.status, m.needs_trust));
                let needs_trust = status.is_some_and(|(_, trust)| trust);
                let status = status.map(|(status, _)| status);
                let connecting = status == Some(McpStatus::Connecting);
                if status == Some(McpStatus::Connected) {
                    vec![
                        MenuItem::new("disconnect", "Disconnect", "", true),
                        MenuItem::new("tools", "View tools", "", true),
                    ]
                } else if needs_trust {
                    vec![MenuItem::new(
                        "trust",
                        "Trust connect",
                        if connecting {
                            "connecting..."
                        } else {
                            "MCP tools not exposed"
                        },
                        false,
                    )]
                } else {
                    vec![MenuItem::new(
                        "connect",
                        "Connect",
                        if connecting {
                            "connecting..."
                        } else {
                            "MCP tools not exposed"
                        },
                        false,
                    )]
                }
            }
        }
    }

    /// Le menu de complétion est-il ouvert ? (au moins un item à proposer).
    pub fn menu_open(&self) -> bool {
        !self.menu_items().is_empty()
    }

    /// Aucune conversation encore (transcript vide) : le rendu affiche l'écran
    /// d'accueil (carte + logo) au lieu du fil. Repart à l'accueil après `/new`
    /// ou `/clear`, qui vident `blocks`.
    pub fn is_welcome(&self) -> bool {
        self.blocks.is_empty() && !self.shutdown_in_progress
    }

    /// Provider ciblé par le niveau 3 (`/providers subscription <provider> …`).
    fn active_provider(&self) -> String {
        self.input
            .strip_prefix("/providers subscription ")
            .and_then(|r| r.split(' ').next())
            .unwrap_or("")
            .to_string()
    }

    /// Serveur MCP ciblé par le niveau 2 (`/mcp <serveur> …`). Le nom peut contenir
    /// des espaces : on retient le plus long nom connu qui préfixe la saisie et est
    /// suivi d'un espace.
    fn active_mcp_server(&self) -> String {
        let Some(rest) = self.input.strip_prefix("/mcp ") else {
            return String::new();
        };
        self.mcp_servers
            .iter()
            .map(|m| m.name.as_str())
            .filter(|name| rest.strip_prefix(*name).is_some_and(|r| r.starts_with(' ')))
            .max_by_key(|name| name.len())
            .unwrap_or("")
            .to_string()
    }

    fn active_file_query(&self) -> Option<(usize, &str)> {
        let prefix = self.input.get(..self.cursor).unwrap_or(&self.input);
        let start = prefix
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(idx, ch)| idx + ch.len_utf8())
            .unwrap_or(0);
        let token = &prefix[start..];
        token.strip_prefix('@').map(|query| (start, query))
    }

    fn replace_file_mention(&mut self, path: &str) {
        let Some((start, _)) = self.active_file_query() else {
            return;
        };
        let replacement = format!("@{path} ");
        self.input.replace_range(start..self.cursor, &replacement);
        self.cursor = start + replacement.len();
    }

    /// Tab : complète le fil d'Ariane vers l'item sélectionné (descend d'un
    /// niveau pour les items à sous-menu, sinon pré-remplit la commande).
    fn complete(&mut self, kind: Menu, item: &MenuItem) {
        let provider = self.active_provider();
        let value = match kind {
            Menu::Commands => format!("{} ", item.id),
            Menu::Models => format!("/models {}", item.id),
            Menu::Permissions => format!("/permissions {}", item.id),
            Menu::Skills => format!("/{} ", item.id),
            Menu::ProviderAuth if item.id == "subscription" => "/providers subscription ".into(),
            Menu::ProviderAuth => format!("/providers {} ", item.id),
            // Provider branché → descend aux actions ; sinon pré-remplit.
            Menu::ProviderList if item.enabled => format!("/providers subscription {} ", item.id),
            Menu::ProviderList => format!("/providers subscription {}", item.id),
            Menu::ProviderActions => format!("/providers subscription {provider} {}", item.id),
            Menu::Files if item.enabled => {
                self.replace_file_mention(&item.id);
                return;
            }
            Menu::Files => return,
            Menu::McpList if item.enabled => format!("/mcp {} ", item.id),
            Menu::McpActions if !item.enabled => return,
            Menu::McpActions => format!("/mcp {} {}", self.active_mcp_server(), item.id),
            Menu::McpList | Menu::Resume | Menu::None => return,
        };
        self.set_input(value);
    }

    /// Entrée : exécute l'item sélectionné — ou descend d'un niveau s'il ouvre un
    /// sous-menu (commande à argument, `subscription`), ou insère (skill).
    fn activate(&mut self, kind: Menu, item: MenuItem) -> InputAction {
        match kind {
            Menu::None => InputAction::None,
            Menu::Commands => {
                if command_takes_arg(&item.id) {
                    self.set_input(format!("{} ", item.id));
                    InputAction::None
                } else {
                    self.clear_input();
                    InputAction::Command(item.id)
                }
            }
            Menu::Models => {
                self.clear_input();
                InputAction::Command(format!("/models {}", item.id))
            }
            Menu::Permissions => {
                self.clear_input();
                InputAction::Command(format!("/permissions {}", item.id))
            }
            Menu::Resume => {
                self.clear_input();
                InputAction::Command(format!("/resume {}", item.id))
            }
            Menu::Skills => {
                // INSERTION (pas d'exécution) : `/<skill> ` remplace le `/skills…`
                // tapé, curseur juste après — l'utilisateur poursuit son message.
                self.set_input(format!("/{} ", item.id));
                InputAction::None
            }
            Menu::Files if item.enabled => {
                self.replace_file_mention(&item.id);
                InputAction::None
            }
            Menu::Files => InputAction::None,
            Menu::ProviderAuth if item.id == "subscription" => {
                self.set_input("/providers subscription ".into());
                InputAction::None
            }
            Menu::ProviderAuth => {
                self.clear_input();
                InputAction::Command(format!("/providers {}", item.id))
            }
            Menu::ProviderList if item.enabled => {
                // Provider branché → descend au menu d'actions (connect/disconnect).
                self.set_input(format!("/providers subscription {} ", item.id));
                InputAction::None
            }
            Menu::ProviderList => {
                self.clear_input();
                InputAction::Command(format!("/providers subscription {}", item.id))
            }
            Menu::ProviderActions => {
                let provider = self.active_provider();
                self.clear_input();
                InputAction::Command(format!("/providers subscription {provider} {}", item.id))
            }
            // Sélectionner un serveur → descend au menu d'actions (connect/disconnect).
            Menu::McpList if item.enabled => {
                self.set_input(format!("/mcp {} ", item.id));
                InputAction::None
            }
            Menu::McpList => InputAction::None,
            Menu::McpActions if !item.enabled => InputAction::None,
            Menu::McpActions => {
                let server = self.active_mcp_server();
                self.clear_input();
                InputAction::Command(format!("/mcp {server} {}", item.id))
            }
        }
    }

    /// Gestion clavier. En attente de permission, seules o/n/Enter/Esc/Ctrl+C comptent.
    pub fn on_key(&mut self, key: KeyEvent) -> InputAction {
        let is_ctrl_c = is_ctrl_key(&key, 'c');
        let is_ctrl_t = is_ctrl_key(&key, 't');
        if !is_ctrl_c {
            self.clear_quit_shortcut_hint();
        }

        if self.transcript_overlay_open {
            return self.on_transcript_overlay_key(key, is_ctrl_t, is_ctrl_c);
        }

        if is_ctrl_t && !self.shutdown_in_progress {
            self.open_transcript_overlay();
            return InputAction::None;
        }

        if self.pending.is_some() {
            return match key.code {
                KeyCode::Char('o') | KeyCode::Char('y') | KeyCode::Enter => {
                    self.pending = None;
                    InputAction::Permission(true)
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.pending = None;
                    InputAction::Permission(false)
                }
                _ if is_ctrl_c => {
                    self.pending = None;
                    self.clear_quit_shortcut_hint();
                    InputAction::Permission(false)
                }
                _ => InputAction::None,
            };
        }

        // Menu de complétion ouvert (commandes ou sous-menus) : flèches / Tab /
        // Entrée / Esc lui sont dédiés.
        if self.menu_open() {
            let items = self.menu_items();
            let idx = self.completion_index.min(items.len().saturating_sub(1));
            let kind = self.menu_kind();
            match key.code {
                KeyCode::Up => {
                    self.completion_index = idx.saturating_sub(1);
                    return InputAction::None;
                }
                KeyCode::Down => {
                    self.completion_index = (idx + 1).min(items.len().saturating_sub(1));
                    return InputAction::None;
                }
                KeyCode::Tab => {
                    if let Some(item) = items.get(idx) {
                        self.complete(kind, item);
                        self.completion_index = 0;
                    }
                    return InputAction::None;
                }
                KeyCode::Enter => {
                    self.completion_index = 0;
                    if let Some(item) = items.get(idx).cloned() {
                        return self.activate(kind, item);
                    }
                    return InputAction::None;
                }
                KeyCode::Esc => {
                    self.clear_input();
                    self.completion_index = 0;
                    return InputAction::None;
                }
                _ if is_ctrl_c => {
                    self.clear_input();
                    self.completion_index = 0;
                    self.clear_quit_shortcut_hint();
                    return InputAction::None;
                }
                _ => {}
            }
        }

        if is_ctrl_c {
            return self.on_ctrl_c();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('a') => {
                    self.move_home();
                    InputAction::None
                }
                KeyCode::Char('e') => {
                    self.move_end();
                    InputAction::None
                }
                KeyCode::Char('u') => {
                    self.clear_input();
                    self.completion_index = 0;
                    InputAction::None
                }
                KeyCode::Char('w') => {
                    self.delete_prev_word();
                    self.completion_index = 0;
                    InputAction::None
                }
                _ => InputAction::None,
            };
        }

        match key.code {
            KeyCode::Esc if self.status == Status::Thinking && key.modifiers.is_empty() => {
                InputAction::Interrupt
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    InputAction::None
                } else if is_command(&text) {
                    // Vraie commande Pyxis (1er mot dans COMMANDS, ex `/models …`).
                    self.clear_input();
                    self.completion_index = 0;
                    InputAction::Command(text)
                } else {
                    // Tout le reste (dont un message commençant par `/<skill> …`)
                    // est envoyé à l'agent.
                    self.clear_input();
                    InputAction::Submit(text)
                }
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                self.completion_index = 0;
                InputAction::None
            }
            KeyCode::Backspace => {
                self.backspace();
                self.completion_index = 0;
                InputAction::None
            }
            KeyCode::Delete => {
                self.delete();
                self.completion_index = 0;
                InputAction::None
            }
            // Déplacements du curseur dans l'input.
            KeyCode::Left => {
                self.move_left();
                InputAction::None
            }
            KeyCode::Right => {
                self.move_right();
                InputAction::None
            }
            KeyCode::Home => {
                self.move_home();
                InputAction::None
            }
            KeyCode::End => {
                self.move_end();
                InputAction::None
            }
            // Flèches (menu fermé) : navigation de l'historique des prompts.
            KeyCode::Up => {
                self.history_prev();
                InputAction::None
            }
            KeyCode::Down => {
                self.history_next();
                InputAction::None
            }
            KeyCode::PageUp => {
                self.scroll_up(5);
                InputAction::ScrollUp
            }
            KeyCode::PageDown => {
                self.scroll_down(5);
                InputAction::ScrollDown
            }
            _ => InputAction::None,
        }
    }

    fn on_transcript_overlay_key(
        &mut self,
        key: KeyEvent,
        is_ctrl_t: bool,
        is_ctrl_c: bool,
    ) -> InputAction {
        if is_ctrl_t || is_ctrl_c || is_plain_char_key(&key, 'q') || key.code == KeyCode::Esc {
            self.close_transcript_overlay();
            self.clear_quit_shortcut_hint();
            return InputAction::None;
        }

        let page = self.transcript_overlay_page_height();
        match key.code {
            KeyCode::Up if key.modifiers.is_empty() => self.transcript_overlay_scroll_up(1),
            KeyCode::Down if key.modifiers.is_empty() => self.transcript_overlay_scroll_down(1),
            KeyCode::PageUp => self.transcript_overlay_scroll_up(page),
            KeyCode::PageDown => self.transcript_overlay_scroll_down(page),
            KeyCode::Home => self.jump_transcript_overlay_top(),
            KeyCode::End => self.jump_transcript_overlay_bottom(),
            KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.transcript_overlay_scroll_up(page)
            }
            KeyCode::Char(' ') if key.modifiers.is_empty() => {
                self.transcript_overlay_scroll_down(page)
            }
            _ if is_plain_char_key(&key, 'k') => self.transcript_overlay_scroll_up(1),
            _ if is_plain_char_key(&key, 'j') => self.transcript_overlay_scroll_down(1),
            _ if is_ctrl_key(&key, 'b') => self.transcript_overlay_scroll_up(page),
            _ if is_ctrl_key(&key, 'f') => self.transcript_overlay_scroll_down(page),
            _ if is_ctrl_key(&key, 'u') => {
                self.transcript_overlay_scroll_up((page.saturating_add(1)) / 2)
            }
            _ if is_ctrl_key(&key, 'd') => {
                self.transcript_overlay_scroll_down((page.saturating_add(1)) / 2)
            }
            _ => {}
        }
        InputAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::event::{ToolCallView, ToolResultView};

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn streamed_text_accumulates_into_one_assistant_block() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::Text("Bon".into()));
        s.apply(&AgentEvent::Text("jour".into()));
        assert_eq!(s.blocks.len(), 1);
        assert_eq!(
            s.blocks[0],
            Block::Assistant {
                text: "Bonjour".into(),
                streaming: true
            }
        );
        s.apply(&AgentEvent::EndTurn);
        assert!(matches!(
            s.blocks[0],
            Block::Assistant {
                streaming: false,
                ..
            }
        ));
        assert_eq!(s.status, Status::Idle);
    }

    #[test]
    fn stream_reset_removes_uncommitted_blocks() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::Text("prefix".into()));
        s.apply(&AgentEvent::Reasoning("raison".into()));
        s.apply(&AgentEvent::StreamReset);
        assert!(s.blocks.is_empty());
        assert_eq!(s.turn_chars, 0);
        s.apply(&AgentEvent::Text("final".into()));
        s.apply(&AgentEvent::EndTurn);
        assert_eq!(
            s.blocks,
            vec![Block::Assistant {
                text: "final".into(),
                streaming: false
            }]
        );
    }

    #[test]
    fn tool_call_finalizes_assistant_and_records_summary() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::Text("je lance".into()));
        s.apply(&AgentEvent::ToolCall(ToolCallView {
            id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({ "command": "ls -la" }),
        }));
        assert!(matches!(
            s.blocks[0],
            Block::Assistant {
                streaming: false,
                ..
            }
        ));
        assert_eq!(
            s.blocks[1],
            Block::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({ "command": "ls -la" }),
                input_hash: crate::cache::value_hash(&serde_json::json!({ "command": "ls -la" })),
            }
        );
    }

    #[test]
    fn tool_result_carries_taint_and_error() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::ToolResult(ToolResultView {
            id: "c1".into(),
            content: "oops".into(),
            is_error: true,
            untrusted: true,
            error_kind: None,
        }));
        assert_eq!(
            s.blocks[0],
            Block::ToolResult {
                call_id: "c1".into(),
                content: "oops".into(),
                untrusted: true,
                is_error: true,
                error_kind: None
            }
        );
    }

    #[test]
    fn typing_and_submit_produces_action_and_clears_input() {
        let mut s = AppState::new("gpt-5", false);
        for c in "salut".chars() {
            assert_eq!(s.on_key(key(c)), InputAction::None);
        }
        assert_eq!(s.input, "salut");
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::Submit("salut".into()));
        assert!(s.input.is_empty());
    }

    #[test]
    fn empty_submit_is_noop() {
        let mut s = AppState::new("gpt-5", false);
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::None
        );
    }

    #[test]
    fn slash_opens_and_filters_command_menu() {
        let mut s = AppState::new("gpt-5", false);
        s.on_key(key('/'));
        assert!(s.menu_open(), "menu should open on /");
        assert_eq!(s.menu_items().len(), COMMANDS.len());
        s.on_key(key('m'));
        // «/m» matche /models ET /mcp.
        let m = s.menu_items();
        assert_eq!(m.len(), 2, "/m matches /models and /mcp");
        assert!(m.iter().all(|it| it.id.starts_with("/m")));
        // «/mo» désambiguïse vers /models seul.
        s.on_key(key('o'));
        let m = s.menu_items();
        assert_eq!(m.len(), 1, "«/mo» ne matche que /models");
        assert_eq!(m[0].id, "/models");
    }

    #[test]
    fn permissions_submenu_marks_current_and_routes_selection() {
        let mut s = AppState::new("gpt-5", false);
        s.set_permission_mode("read-only");
        s.set_input("/permissions ".into());

        let items = s.menu_items();
        assert_eq!(items.len(), PERMISSION_MODES.len());
        let current = items.iter().find(|item| item.id == "read-only").unwrap();
        assert!(current.label.contains("(current)"));

        s.set_input("/permissions full".into());
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "full-access");
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::Command("/permissions full-access".into())
        );
    }

    #[test]
    fn mcp_submenu_lists_servers_with_status_badges() {
        let mut s = AppState::new("gpt-5", false);
        s.mcp_servers = vec![
            McpServerMeta {
                name: "filesystem".into(),
                status: McpStatus::Connected,
                source: "workspace".into(),
                needs_trust: false,
                tool_count: 3,
            },
            McpServerMeta {
                name: "fetch".into(),
                status: McpStatus::Disconnected,
                source: "user".into(),
                needs_trust: false,
                tool_count: 0,
            },
        ];
        for c in "/mcp ".chars() {
            s.on_key(key(c));
        }
        let items = s.menu_items();
        assert_eq!(items.len(), 2);
        let fs = items.iter().find(|i| i.id == "filesystem").unwrap();
        assert!(fs.hint.contains("connected"), "connected status expected");
        assert!(fs.hint.contains("3 tools"));
        let fetch = items.iter().find(|i| i.id == "fetch").unwrap();
        assert_eq!(fetch.hint, "user · not connected");
    }

    #[test]
    fn mcp_server_selection_descends_to_disabled_connect() {
        let mut s = AppState::new("gpt-5", false);
        s.mcp_servers = vec![McpServerMeta {
            name: "fetch".into(),
            status: McpStatus::Disconnected,
            source: "user".into(),
            needs_trust: false,
            tool_count: 0,
        }];
        for c in "/mcp ".chars() {
            s.on_key(key(c));
        }
        // Entrée sur le serveur → descend au menu d'actions (n'exécute pas).
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::None);
        assert_eq!(s.input, "/mcp fetch ");
        // Déconnecté: connect visible mais inactif, car les outils MCP ne sont
        // pas exposés au modèle dans ce build.
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "connect");
        assert!(!items[0].enabled);
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::None);
    }

    #[test]
    fn mcp_workspace_server_routes_through_trust_action() {
        let mut s = AppState::new("gpt-5", false);
        s.mcp_servers = vec![McpServerMeta {
            name: "local".into(),
            status: McpStatus::Disconnected,
            source: "workspace".into(),
            needs_trust: true,
            tool_count: 0,
        }];
        s.set_input("/mcp local ".into());
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "trust");
        assert!(!items[0].enabled);
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::None);
    }

    #[test]
    fn mcp_connected_server_offers_disconnect_and_tools() {
        let mut s = AppState::new("gpt-5", false);
        s.mcp_servers = vec![McpServerMeta {
            name: "fs".into(),
            status: McpStatus::Connected,
            source: "workspace".into(),
            needs_trust: false,
            tool_count: 2,
        }];
        s.set_input("/mcp fs ".into());
        let ids: Vec<_> = s.menu_items().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec!["disconnect", "tools"]);
    }

    #[test]
    fn mcp_server_name_with_space_reaches_actions() {
        let mut s = AppState::new("gpt-5", false);
        s.mcp_servers = vec![McpServerMeta {
            name: "my server".into(),
            status: McpStatus::Connected,
            source: "workspace".into(),
            needs_trust: false,
            tool_count: 1,
        }];
        // complete() écrit le nom complet (avec espace) ; le menu doit basculer en
        // actions, pas rester bloqué sur la liste (régression review #7).
        s.set_input("/mcp my server ".into());
        let ids: Vec<_> = s.menu_items().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec!["disconnect", "tools"]);
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            InputAction::Command("/mcp my server disconnect".into())
        );
    }

    #[test]
    fn mcp_empty_registry_shows_disabled_placeholder() {
        let mut s = AppState::new("gpt-5", false);
        for c in "/mcp ".chars() {
            s.on_key(key(c));
        }
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert!(!items[0].enabled, "placeholder non sélectionnable");
        // Entrée sur le placeholder ne dispatche rien.
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::None);
    }

    #[test]
    fn enter_on_non_arg_command_executes() {
        let mut s = AppState::new("gpt-5", false);
        s.on_key(key('/'));
        // Navigue jusqu'à /quit (sans dépendre de l'ordre exact de COMMANDS).
        let quit_idx = COMMANDS.iter().position(|(n, _, _)| *n == "/quit").unwrap();
        for _ in 0..quit_idx {
            s.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command("/quit".into()));
        assert!(s.input.is_empty());
    }

    #[test]
    fn goal_command_highlighted_and_routed() {
        // `/goal` est une vraie commande (routée), pas un message agent.
        let mut s = AppState::new("gpt-5", false);
        for c in "/goal vivre de mes produits".chars() {
            s.on_key(key(c));
        }
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            InputAction::Command("/goal vivre de mes produits".into())
        );
    }

    #[test]
    fn skills_submenu_inserts_and_routes_to_agent() {
        let mut s = AppState::new("gpt-5", false);
        s.skills = vec!["frontend-design".into(), "meta-code".into()];
        // Ouvre le sous-menu skills, filtre par sous-chaîne.
        s.input = "/skills front".into();
        s.cursor = s.input.len();
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "frontend-design");
        // Sélection → INSÈRE `/frontend-design ` (pas de Command), curseur en fin.
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::None);
        assert_eq!(s.input, "/frontend-design ");
        assert_eq!(s.cursor, s.input.len());
        // Soumis avec un message → part à l'AGENT (pas une commande Pyxis).
        for c in "refais l'UI".chars() {
            s.on_key(key(c));
        }
        let submit = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            submit,
            InputAction::Submit("/frontend-design refais l'UI".into())
        );
    }

    #[test]
    fn file_mentions_filter_insert_and_submit_to_agent() {
        let mut s = AppState::new("gpt-5", false);
        s.files = vec!["crates/agent-tui/src/state.rs".into(), "README.md".into()];
        s.set_input("@state".into());

        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "crates/agent-tui/src/state.rs");
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.input, "@crates/agent-tui/src/state.rs ");

        for c in "explique".chars() {
            s.on_key(key(c));
        }
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::Submit("@crates/agent-tui/src/state.rs explique".into())
        );
    }

    #[test]
    fn cursor_inserts_in_middle_and_moves() {
        let mut s = AppState::new("gpt-5", false);
        for c in "helo".chars() {
            s.on_key(key(c));
        }
        // curseur en fin (4) ; recule de 1 (entre 'l' et 'o') et insère 'l'.
        s.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        s.on_key(key('l'));
        assert_eq!(s.input, "hello");
        assert_eq!(s.cursor, 4);
        // Home puis Backspace ne fait rien (curseur en tête).
        s.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        s.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(s.input, "hello");
        // Delete supprime le char sous le curseur ('h').
        s.on_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
        assert_eq!(s.input, "ello");
    }

    #[test]
    fn unicode_cursor_moves_and_deletes_graphemes() {
        let mut s = AppState::new("gpt-5", false);
        s.insert_str("a¢🙂");
        assert_eq!(s.cursor, "a¢🙂".len());

        s.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(s.cursor, "a¢".len());

        s.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(s.input, "a🙂");
        assert_eq!(s.cursor, "a".len());
    }

    #[test]
    fn ctrl_shortcuts_edit_without_inserting_control_chars() {
        let mut s = AppState::new("gpt-5", false);
        s.insert_str("hello world");

        s.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        s.on_key(key('>'));
        assert_eq!(s.input, ">hello world");

        s.on_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        s.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(s.input, ">hello ");

        s.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(s.input.is_empty());
    }

    #[test]
    fn providers_menu_three_levels_and_badge() {
        let mut s = AppState::new("gpt-5", true);
        s.provider_connected = true;
        // Niveau 1 : types d'auth.
        s.input = "/providers ".into();
        let lvl1 = s.menu_items();
        assert_eq!(lvl1.len(), AUTH_KINDS.len());
        assert_eq!(lvl1[0].id, "subscription");
        assert!(!lvl1[1].enabled, "API key inactive");
        // « subscription » descend au niveau 2 (providers).
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.input, "/providers subscription ");
        let lvl2 = s.menu_items();
        assert_eq!(lvl2[0].id, "codex");
        assert_eq!(lvl2[0].hint, "connected", "connected badge on codex");
        // Codex (branché) descend au niveau 3 (actions).
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.input, "/providers subscription codex ");
        let lvl3 = s.menu_items();
        // Connecté → Connect grisé, Disconnect actif.
        assert_eq!(lvl3[0].id, "connect");
        assert!(!lvl3[0].enabled, "Connect disabled while connected");
        assert_eq!(lvl3[1].id, "disconnect");
        assert!(lvl3[1].enabled, "Disconnect enabled while connected");
        // Sélectionner Disconnect → exécute la commande pleine.
        s.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            InputAction::Command("/providers subscription codex disconnect".into())
        );
    }

    #[test]
    fn provider_actions_invert_when_disconnected() {
        let mut s = AppState::new("gpt-5", true);
        s.provider_connected = false;
        s.input = "/providers subscription codex ".into();
        let lvl3 = s.menu_items();
        assert!(lvl3[0].enabled, "Connect enabled while disconnected");
        assert!(!lvl3[1].enabled, "Disconnect disabled while disconnected");
    }

    #[test]
    fn arrow_keys_navigate_prompt_history() {
        let mut s = AppState::new("gpt-5", false);
        s.push_user("premier");
        s.push_user("second");
        // brouillon en cours de frappe
        for c in "brou".chars() {
            s.on_key(key(c));
        }
        let up = || KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let down = || KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        // Haut → plus récent ; le brouillon est sauvegardé.
        s.on_key(up());
        assert_eq!(s.input, "second");
        s.on_key(up());
        assert_eq!(s.input, "premier");
        s.on_key(up()); // bloqué sur le plus ancien (pas de wrap)
        assert_eq!(s.input, "premier");
        s.on_key(down());
        assert_eq!(s.input, "second");
        s.on_key(down()); // au-delà du récent → brouillon restauré
        assert_eq!(s.input, "brou");
    }

    #[test]
    fn history_ignores_consecutive_duplicates() {
        let mut s = AppState::new("gpt-5", false);
        s.push_user("x");
        s.push_user("x");
        s.push_user("y");
        assert_eq!(s.history, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn prompts_from_messages_keeps_user_only() {
        let msgs = vec![
            Message::user("q1"),
            Message::assistant_text("a1"),
            Message::user("q2"),
        ];
        assert_eq!(
            prompts_from_messages(&msgs),
            vec!["q1".to_string(), "q2".to_string()]
        );
    }

    #[test]
    fn resume_submenu_lists_sessions_and_routes_id() {
        let mut s = AppState::new("gpt-5", false);
        s.sessions = vec![
            SessionMeta {
                id: "111.jsonl".into(),
                label: "Explique le projet".into(),
                hint: "3 msg · il y a 1 h".into(),
            },
            SessionMeta {
                id: "222.jsonl".into(),
                label: "Refactor lexer".into(),
                hint: "8 msg · il y a 2 j".into(),
            },
        ];
        s.input = "/resume ".into();
        assert!(s.menu_open());
        assert_eq!(s.menu_items().len(), 2);
        s.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // → 2e session
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command("/resume 222.jsonl".into()));
    }

    #[test]
    fn resume_submenu_filters_or_falls_back_to_manual_id() {
        let mut s = AppState::new("gpt-5", false);
        s.sessions = vec![
            SessionMeta {
                id: "111.jsonl".into(),
                label: "Alpha".into(),
                hint: "".into(),
            },
            SessionMeta {
                id: "222.jsonl".into(),
                label: "Beta".into(),
                hint: "".into(),
            },
        ];

        s.set_input("/resume 222".into());
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "222.jsonl");

        s.set_input("/resume missing.jsonl".into());
        assert!(!s.menu_open());
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::Command("/resume missing.jsonl".into())
        );
    }

    #[test]
    fn blocks_from_messages_rebuilds_transcript() {
        let msgs = vec![
            Message::user("salut"),
            Message::assistant(vec![
                ContentBlock::Text {
                    text: "voici".into(),
                },
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": "a.rs" }),
                },
            ]),
            Message::tool_result("c1", "contenu", false),
        ];
        let blocks = blocks_from_messages(&msgs);
        assert!(matches!(&blocks[0], Block::User(t) if t == "salut"));
        assert!(matches!(&blocks[1], Block::Assistant { text, .. } if text == "voici"));
        assert!(matches!(&blocks[2], Block::ToolCall { name, .. } if name == "read"));
        assert!(matches!(&blocks[3], Block::ToolResult { content, .. } if content == "contenu"));
    }

    #[test]
    fn models_submenu_opens_and_selection_routes_command() {
        let mut s = AppState::new("gpt-5", false);
        s.on_key(key('/'));
        s.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // → /models
        // Entrée sur une commande à argument OUVRE le sous-menu (n'exécute pas).
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.input, "/models ");
        assert!(s.menu_open());
        assert_eq!(s.menu_items().len(), MODELS.len());
        // Naviguer puis sélectionner un modèle → exécute `/models <slug>`.
        s.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // → gpt-5.4
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command("/models gpt-5.4".into()));
    }

    #[test]
    fn models_submenu_accepts_custom_slug() {
        let mut s = AppState::new("gpt-5", false);
        s.set_input("/models gpt-6-preview".into());
        let items = s.menu_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "gpt-6-preview");
        assert_eq!(items[0].hint, "custom");
        let action = s.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::Command("/models gpt-6-preview".into()));
    }

    #[test]
    fn tab_completes_command_name() {
        let mut s = AppState::new("gpt-5", false);
        s.on_key(key('/'));
        s.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)); // complète /help + espace
        assert_eq!(s.input, "/help ");
        assert!(
            !s.menu_open(),
            "espace présent (commande sans sous-menu) → fermé"
        );
    }

    #[test]
    fn permission_mode_routes_keys() {
        let mut s = AppState::new("gpt-5", false);
        s.pending = Some(PermissionPrompt::new(
            "bash",
            "sensible",
            crate::diff::Diff::default(),
        ));
        // une frappe normale ne tape PAS dans l'input pendant la confirmation
        assert_eq!(s.on_key(key('x')), InputAction::None);
        assert!(s.input.is_empty());
        // 'o' accepte
        assert_eq!(s.on_key(key('o')), InputAction::Permission(true));
        assert!(s.pending.is_none());
    }

    #[test]
    fn plain_esc_interrupts_running_turn_without_modal() {
        let mut s = AppState::new("gpt-5", false);
        s.push_user("work");

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            InputAction::Interrupt
        );
    }

    #[test]
    fn esc_keeps_permission_priority_over_interrupt() {
        let mut s = AppState::new("gpt-5", false);
        s.push_user("work");
        s.pending = Some(PermissionPrompt::new(
            "bash",
            "needs approval",
            crate::diff::Diff::default(),
        ));

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            InputAction::Permission(false)
        );
    }

    #[test]
    fn interrupted_event_clears_pending_and_returns_idle() {
        let mut s = AppState::new("gpt-5", false);
        s.apply(&AgentEvent::Text("partial".into()));
        s.pending = Some(PermissionPrompt::new(
            "bash",
            "needs approval",
            crate::diff::Diff::default(),
        ));

        s.apply(&AgentEvent::Interrupted);

        assert!(s.pending.is_none());
        assert_eq!(s.status, Status::Idle);
        assert!(matches!(
            s.blocks.last(),
            Some(Block::Notice(message)) if message == "interrupted"
        ));
        assert!(matches!(
            s.blocks.first(),
            Some(Block::Assistant {
                streaming: false,
                ..
            })
        ));
    }

    #[test]
    fn first_ctrl_c_arms_quit_shortcut_second_ctrl_c_quits() {
        let mut s = AppState::new("gpt-5", false);
        let action = s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, InputAction::None);
        assert!(!s.should_quit);
        assert!(s.quit_shortcut_hint_visible());

        let action = s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, InputAction::Quit);
        assert!(s.should_quit);
        assert!(!s.quit_shortcut_hint_visible());
    }

    #[test]
    fn shutdown_feedback_clears_modal_and_footer_hint() {
        let mut s = AppState::new("gpt-5", false);
        s.pending = Some(PermissionPrompt::new(
            "bash",
            "needs approval",
            crate::diff::Diff::default(),
        ));
        s.arm_quit_shortcut();

        s.show_shutdown_in_progress();

        assert!(s.shutdown_in_progress());
        assert!(s.pending.is_none());
        assert_eq!(s.status, Status::Idle);
        assert!(!s.quit_shortcut_hint_visible());
        assert!(!s.is_welcome());
    }

    #[test]
    fn ctrl_c_interrupts_running_turn_before_quit() {
        let mut s = AppState::new("gpt-5", false);
        s.push_user("work");

        let action = s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, InputAction::Interrupt);
        assert!(s.quit_shortcut_hint_visible());
        assert!(!s.should_quit);

        s.apply(&AgentEvent::Interrupted);
        let action = s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, InputAction::Quit);
        assert!(s.should_quit);
    }

    #[test]
    fn ctrl_c_keeps_permission_priority_over_interrupt() {
        let mut s = AppState::new("gpt-5", false);
        s.push_user("work");
        s.pending = Some(PermissionPrompt::new(
            "bash",
            "needs approval",
            crate::diff::Diff::default(),
        ));

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            InputAction::Permission(false)
        );
        assert!(!s.should_quit);
        assert!(!s.quit_shortcut_hint_visible());
    }

    #[test]
    fn ctrl_c_dismisses_menu_before_quit_shortcut() {
        let mut s = AppState::new("gpt-5", false);
        s.on_key(key('/'));

        assert!(s.menu_open());
        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            InputAction::None
        );
        assert!(s.input.is_empty());
        assert!(!s.menu_open());
        assert!(!s.should_quit);
        assert!(!s.quit_shortcut_hint_visible());
    }

    #[test]
    fn ctrl_t_opens_and_closes_transcript_overlay() {
        let mut s = AppState::new("gpt-5", false);
        s.input = "draft".into();
        s.cursor = s.input.len();

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            InputAction::None
        );
        assert!(s.transcript_overlay_open());
        assert_eq!(s.input, "draft");

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            InputAction::None
        );
        assert!(!s.transcript_overlay_open());
        assert_eq!(s.input, "draft");
    }

    #[test]
    fn transcript_overlay_routes_pager_keys_without_editing_input() {
        let mut s = AppState::new("gpt-5", false);
        s.set_transcript_overlay_metrics(120, 20);
        s.open_transcript_overlay();
        s.input = "draft".into();
        s.cursor = s.input.len();

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.transcript_overlay_scroll(), 20);

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            InputAction::None
        );
        assert_eq!(s.transcript_overlay_scroll(), 10);

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.transcript_overlay_scroll(), 120);

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            InputAction::None
        );
        assert_eq!(s.input, "draft");

        assert_eq!(
            s.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            InputAction::None
        );
        assert!(!s.transcript_overlay_open());
    }

    // US-044/045 : cycle de vie de la progression d'un tour.
    #[test]
    fn turn_progress_lifecycle() {
        let mut s = AppState::new("gpt-5", true);
        s.begin_turn();
        assert_eq!(s.turn_chars, 0);
        assert!(s.turn_elapsed.is_none());
        s.apply(&AgentEvent::Text("abcd".into()));
        assert_eq!(s.turn_chars, 4, "chars cumulés pour l'estimation de tokens");
        s.tick_progress(std::time::Duration::from_secs(5));
        assert_eq!(s.turn_elapsed, Some(std::time::Duration::from_secs(5)));
        assert_eq!(s.spinner_tick, 1, "le tick avance l'animation");
        s.end_turn();
        assert!(
            s.turn_elapsed.is_none(),
            "indicateurs disparus en fin de tour"
        );
    }

    // US-046 : `unseen` ne compte que les blocs arrivés en scroll haut, et se remet
    // à zéro au retour en bas (auto-follow).
    #[test]
    fn unseen_tracks_scrolled_up_content() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text("a".into()));
        s.apply(&AgentEvent::EndTurn);
        assert_eq!(s.unseen, 0, "collé en bas : rien d'unseen");
        s.scroll = 2; // l'utilisateur a remonté
        s.apply(&AgentEvent::Text("b".into())); // nouveau bloc → +1
        assert_eq!(s.unseen, 1);
        s.scroll_down(5); // retour au bas
        assert_eq!(s.scroll, 0);
        assert_eq!(s.unseen, 0, "auto-follow → reset");
    }

    // US-046 (robustesse) : quitter le bas écarte un `unseen` périmé (ex. laissé par
    // un `scroll = 0` direct du chemin commande, qui ne passe pas par scroll_down).
    #[test]
    fn scroll_up_clears_stale_unseen() {
        let mut s = AppState::new("gpt-5", true);
        s.scroll_max.set(50); // du contenu scrollable
        s.unseen = 3; // périmé, alors qu'on est collé en bas
        s.scroll_up(5); // on quitte le bas → compteur vierge
        assert!(s.scroll > 0);
        assert_eq!(s.unseen, 0, "compteur périmé écarté en quittant le bas");
    }

    // US-046 : un stream qui APPEND au dernier bloc Assistant (sans créer de nouveau
    // bloc) signale quand même du contenu si l'utilisateur a remonté le transcript.
    #[test]
    fn unseen_floors_on_pure_stream_append() {
        let mut s = AppState::new("gpt-5", true);
        s.apply(&AgentEvent::Text("start ".into()));
        s.scroll = 2; // l'utilisateur remonte PENDANT le stream
        s.apply(&AgentEvent::Text("suite".into())); // APPEND (pas de nouveau bloc)
        assert_eq!(s.blocks.len(), 1, "un seul bloc Assistant (append)");
        assert_eq!(
            s.unseen, 1,
            "stream signals content even without a new block"
        );
    }
}
