//! Erreurs du système d'outils. `ValidationError` est l'échec de `validate_input`
//! (pré-exécution) ; `ToolError` est l'erreur remontée à l'agent (sérialisée en
//! `ToolOutcome { is_error: true }`). Aucune ne panique : le pipeline est
//! fail-closed (ARCHITECTURE §4.1, invariant 4).

use agent_core::ToolErrorKind;

/// Échec de validation d'entrée d'un outil (pré-permission, pré-exécution).
#[derive(Debug, Clone, thiserror::Error)]
#[error("validation: {0}")]
pub struct ValidationError(pub String);

impl ValidationError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// Erreur d'exécution d'un outil. Convertie en `ToolOutcome { is_error: true }`
/// puis renvoyée à l'agent comme `tool_result` (le modèle voit l'échec et peut
/// réagir) — jamais propagée comme panic.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// L'entrée JSON ne se parse pas vers le schéma de l'outil (fail-closed :
    /// on n'exécute pas — US-010 AC3).
    #[error("argument invalide: {0}")]
    Parse(String),
    /// `validate_input` a refusé l'entrée.
    #[error(transparent)]
    Validation(#[from] ValidationError),
    /// Chemin hors du workspace (confinement applicatif ; le kernel renforce via
    /// Landlock en US-020).
    #[error("chemin hors du workspace: {0}")]
    OutsideWorkspace(String),
    /// Erreur d'E/S (fichier introuvable, permission OS, etc.).
    #[error("io: {0}")]
    Io(String),
    /// Entrée rejetée pour une raison métier (ancre ambiguë, fichier binaire…).
    #[error("{0}")]
    Rejected(String),
    /// L'outil a dépassé son timeout (signalé par le Registry, pas par l'outil).
    #[error("timeout dépassé")]
    Timeout,
}

impl ToolError {
    pub fn kind(&self) -> ToolErrorKind {
        match self {
            ToolError::Parse(_) => ToolErrorKind::Parse,
            ToolError::Validation(_) => ToolErrorKind::Validation,
            ToolError::OutsideWorkspace(_) => ToolErrorKind::OutsideWorkspace,
            ToolError::Io(_) => ToolErrorKind::Io,
            ToolError::Rejected(_) => ToolErrorKind::Rejected,
            ToolError::Timeout => ToolErrorKind::Timeout,
        }
    }
}
