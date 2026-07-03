//! Trait `Tool` (fail-closed) + `DynTool` object-safe + adapter (ARCHITECTURE
//! §4.1). Le trait générique porte un type d'entrée associé (non object-safe) ;
//! `DynTool` est le wrapper dyn-compatible stocké dans le `Registry` — du point
//! de vue du dispatch, un outil natif et (à terme) un outil MCP sont
//! indistinguables.
//!
//! Defaults FAIL-CLOSED (invariant 4) : sans override explicite, un outil est
//! supposé non concurrent, mutant, sensible, et à sortie untrusted.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;

use crate::error::{ToolError, ValidationError};
use crate::permission::{PermCtx, PermissionDecision};

/// Durcissement opaque d'une commande shell (Bash) : closure injectée par
/// l'agent-cli, qui applique le sandbox réseau (env `HTTP_PROXY`). Opaque ici
/// pour garder `agent-tools` découplé d'`agent-sandbox` ; le confinement FS
/// Landlock est, lui, process-wide (hérité), donc transparent pour les outils.
pub type CommandHardener = Arc<dyn Fn(&mut tokio::process::Command) + Send + Sync>;

/// Contexte d'exécution partagé passé à chaque outil. `&ToolCtx` (partagé) :
/// les outils concurrents le lisent en parallèle. La mutation d'état d'agent
/// (context-modifiers) est différée (Phase 2).
#[derive(Clone)]
pub struct ToolCtx {
    /// Racine du workspace : ancre des chemins relatifs et frontière de
    /// confinement (renforcée au kernel par Landlock process-wide, US-020).
    pub workspace: PathBuf,
    /// Timeout appliqué par le Registry autour de `call()`.
    pub timeout: Duration,
    /// Durcissement de commande (sandbox réseau Bash), injecté par l'agent-cli.
    pub harden: Option<CommandHardener>,
}

impl std::fmt::Debug for ToolCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCtx")
            .field("workspace", &self.workspace)
            .field("timeout", &self.timeout)
            .field("harden", &self.harden.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

impl ToolCtx {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            timeout: Duration::from_secs(120),
            harden: None,
        }
    }
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
    pub fn with_hardener(mut self, harden: CommandHardener) -> Self {
        self.harden = Some(harden);
        self
    }
}

/// Sortie d'un outil : le texte que le modèle verra comme `tool_result`.
/// `is_error` distingue un échec *sémantique* (commande Bash en exit ≠ 0) d'une
/// vraie erreur de pipeline (`ToolError`) — dans les deux cas le modèle voit le
/// contenu et peut réagir.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    /// Sortie nominale (succès).
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }
    /// Sortie marquée erreur sémantique (le contenu est conservé pour le modèle).
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Trait des outils natifs. Generic sur l'entrée → monomorphisé, branché dans le
/// Registry via `DynToolAdapter`.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Type d'entrée (désérialisé depuis le JSON du `tool_use`).
    type Input: DeserializeOwned + Send;

    fn name(&self) -> &str;
    /// Description fournie au modèle (cappée par le Registry à l'exposition).
    fn description(&self) -> String;
    /// JSON Schema de l'entrée (exposé au modèle dans `ToolSpec`).
    fn input_schema(&self) -> serde_json::Value;

    // ───── Defaults FAIL-CLOSED (invariant 4) — un outil élargit explicitement.
    /// Peut tourner en parallèle d'autres outils (typiquement les reads).
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    /// N'effectue aucune mutation (lecture pure).
    fn is_read_only(&self) -> bool {
        false
    }
    /// Action destructive ou réseau → cible de la défense taint (§4.6) : si du
    /// taint est récent, on force `Ask` même dans un mode permissif.
    fn is_sensitive(&self) -> bool {
        true
    }
    /// Une sortie untrusted récente doit-elle forcer une confirmation avant cet
    /// outil ? Par défaut, toute mutation ou action sensible est protégée.
    fn is_taint_sensitive(&self) -> bool {
        self.is_sensitive() || !self.is_read_only()
    }
    /// Sortie untrusted (taintée) — défaut pour toute sortie d'outil (OWASP
    /// LLM01).
    fn returns_untrusted(&self) -> bool {
        true
    }

    /// Invariants comportementaux co-localisés avec l'outil (US-026) : règles que
    /// le modèle doit connaître pour bien s'en servir (ex. « l'ancre est cherchée
    /// dans le fichier original »). Collectés par le Registry et injectés dans le
    /// system prompt. Défaut : aucune.
    fn behavioral_guidelines(&self) -> &[&'static str] {
        &[]
    }

    /// Validation d'entrée (pré-permission, pré-exécution). Défaut : accepte.
    fn validate_input(&self, _input: &Self::Input) -> Result<(), ValidationError> {
        Ok(())
    }

    /// Décision *baseline* propre à l'outil. Défaut fail-closed : `Ask`.
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Ask
    }

    /// Exécution. Le Registry l'enveloppe déjà dans un `timeout` : un `call` qui
    /// pend ne bloque pas la boucle.
    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

/// Façade object-safe stockée dans le Registry. Le JSON brut traverse ici ; le
/// parse vers `Tool::Input` est interne à l'adapter.
#[async_trait]
pub trait DynTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> String;
    fn input_schema(&self) -> serde_json::Value;
    fn is_concurrency_safe(&self) -> bool;
    fn is_read_only(&self) -> bool;
    fn is_sensitive(&self) -> bool;
    fn is_taint_sensitive(&self) -> bool;
    fn returns_untrusted(&self) -> bool;
    /// Invariants comportementaux de l'outil (US-026), forwardés depuis `Tool`.
    fn behavioral_guidelines(&self) -> &[&'static str];
    /// Parse + `validate_input` SANS exécuter (fail-closed, US-010 AC3). Erreur
    /// ⇒ le Registry renvoie l'échec à l'agent sans appeler `call`.
    fn precheck(&self, raw: &serde_json::Value) -> Result<(), ToolError>;
    /// Décision baseline de l'outil (raw déjà validé par `precheck`).
    fn permission(&self, raw: &serde_json::Value, ctx: &PermCtx) -> PermissionDecision;
    /// Parse + `call`. Enveloppé dans un timeout par le Registry.
    async fn invoke(&self, raw: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

/// Adapter générique `Tool` → `DynTool`.
pub struct DynToolAdapter<T: Tool> {
    inner: T,
}

impl<T: Tool> DynToolAdapter<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl<T: Tool> DynTool for DynToolAdapter<T> {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> String {
        self.inner.description()
    }
    fn input_schema(&self) -> serde_json::Value {
        self.inner.input_schema()
    }
    fn is_concurrency_safe(&self) -> bool {
        self.inner.is_concurrency_safe()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    fn is_sensitive(&self) -> bool {
        self.inner.is_sensitive()
    }
    fn is_taint_sensitive(&self) -> bool {
        self.inner.is_taint_sensitive()
    }
    fn returns_untrusted(&self) -> bool {
        self.inner.returns_untrusted()
    }
    fn behavioral_guidelines(&self) -> &[&'static str] {
        self.inner.behavioral_guidelines()
    }
    fn precheck(&self, raw: &serde_json::Value) -> Result<(), ToolError> {
        let input: T::Input =
            serde_json::from_value(raw.clone()).map_err(|e| ToolError::Parse(e.to_string()))?;
        self.inner.validate_input(&input)?;
        Ok(())
    }
    fn permission(&self, raw: &serde_json::Value, ctx: &PermCtx) -> PermissionDecision {
        // `precheck` a déjà garanti que le parse réussit ; en cas de course
        // improbable, fail-closed → Deny.
        match serde_json::from_value::<T::Input>(raw.clone()) {
            Ok(input) => self.inner.permission(&input, ctx),
            Err(_) => PermissionDecision::Deny,
        }
    }
    async fn invoke(&self, raw: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let input: T::Input =
            serde_json::from_value(raw).map_err(|e| ToolError::Parse(e.to_string()))?;
        self.inner.call(input, ctx).await
    }
}

/// Boîte un outil natif en `DynTool` prêt pour le Registry.
pub fn into_dyn<T: Tool + 'static>(tool: T) -> Box<dyn DynTool> {
    Box::new(DynToolAdapter::new(tool))
}
