//! Contrat de dispatch d'outils (injecté). L'implémentation réelle (registry,
//! permissions, taint, pipeline) est `agent-tools` (EP-003) ; le cœur ne connaît
//! que ce trait. En EP-002, un mock suffit à fermer la boucle stream→outil.

use crate::message::ToolCallId;

/// Un appel d'outil demandé par le modèle (args déjà réassemblés en JSON valide).
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub id: ToolCallId,
    pub name: String,
    pub input: serde_json::Value,
}

/// Résultat d'un outil. `is_error` distingue l'échec applicatif ; `untrusted`
/// porte le taint (OWASP LLM01) décidé par le pipeline `agent-tools` d'après
/// `Tool::returns_untrusted()` — fail-closed à `true` par défaut (US-013).
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub id: ToolCallId,
    pub content: String,
    pub is_error: bool,
    pub untrusted: bool,
}

#[async_trait::async_trait]
pub trait ToolDispatch: Send + Sync {
    /// Exécute un batch d'appels et retourne leurs résultats (ordre non garanti ;
    /// chaque résultat est corrélé par `id`).
    async fn dispatch(&self, calls: Vec<ToolInvocation>) -> Vec<ToolOutcome>;
}
