//! `AgentEvent` — LE contrat cœur → clients (TUI, `-p` headless, Paneflow).
//! Structuré, sérialisable, AUCUNE décision de présentation, JAMAIS d'ANSI
//! (ARCHITECTURE §10.1, invariant 2). Distinct de `StreamEvent` (provider→cœur).

use crate::compaction::CompactKind;
use crate::error::AgentError;
use crate::message::{ToolCallId, ToolErrorKind};
use crate::transition::ExhaustReason;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Le stream courant a été abandonné avant commit (retry/recover).
    /// Les clients doivent retirer les deltas live non finalisés.
    StreamReset,
    /// Delta de texte assistant.
    Text(String),
    /// Delta de raisonnement (si le provider en émet).
    Reasoning(String),
    /// Un outil va s'exécuter.
    ToolCall(ToolCallView),
    /// Fragment de sortie d'un outil encore en cours (US-015). Purement
    /// informatif : le `ToolResult` final reste la seule source du transcript,
    /// et un client qui ignore cette variante garde le comportement d'avant.
    ToolOutputDelta(ToolOutputDeltaView),
    /// Résultat d'outil (le taint vit dans le view-model — US-013).
    ToolResult(ToolResultView),
    /// Une compaction vient d'avoir lieu.
    Compacted(CompactKind),
    /// Un aller-retour modèle vient de se terminer (US-017). Émis après chaque
    /// réponse complète du provider, qu'elle close le tour ou qu'elle enchaîne
    /// sur des outils. Purement informatif : un client qui l'ignore garde le
    /// comportement d'avant.
    ModelTurn(ModelTurnView),
    /// Diff agrégé des fichiers modifiés pendant le tour (US-018). Émis par le
    /// CLIENT à la frontière de fin de tour, pas par la boucle : calculer un diff
    /// suppose de lire le disque, ce que le cœur ne fait pas (invariant 1). Jamais
    /// émis quand rien n'a changé.
    TurnDiff(TurnDiffView),
    /// Demande d'autorisation (émis par le pipeline d'outils — US-013, non par
    /// le cœur en EP-002 ; présent pour fixer le contrat).
    PermissionAsk(PermissionReq),
    EndTurn,
    Interrupted,
    Exhausted(ExhaustReason),
    Error(AgentError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallView {
    pub id: ToolCallId,
    pub name: String,
    pub input: serde_json::Value,
}

/// Fragment de sortie produit par un outil avant sa fin (US-015). `chunk` est du
/// contenu externe : untrusted par construction, comme le résultat final.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutputDeltaView {
    pub id: ToolCallId,
    pub chunk: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultView {
    pub id: ToolCallId,
    pub content: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ToolErrorKind>,
    /// Sortie d'outil = untrusted par défaut (taint, US-013).
    pub untrusted: bool,
}

/// Fin d'un aller-retour modèle (US-017). Les compteurs sont CUMULÉS depuis le
/// début du run : ce sont ceux qui pilotent le budget, donc réels quand le
/// provider rapporte son `usage`, estimés localement sinon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnView {
    /// Indice 1-based du tour modèle qui vient de se terminer.
    pub index: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Diff agrégé d'un tour (US-018). Vide n'est jamais émis.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnDiffView {
    pub files: Vec<FileDiffView>,
}

impl TurnDiffView {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Lignes ajoutées puis retirées, tous fichiers confondus.
    pub fn totals(&self) -> (u32, u32) {
        self.files.iter().fold((0, 0), |(added, removed), file| {
            (
                added.saturating_add(file.added_lines),
                removed.saturating_add(file.removed_lines),
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiffView {
    /// Chemin relatif à la racine du workspace.
    pub path: String,
    pub change: FileChange,
    pub added_lines: u32,
    pub removed_lines: u32,
    /// Diff unifié. Absent pour un fichier binaire ou plus volumineux que le
    /// seuil de diff : le fichier reste listé, son contenu n'est pas comparé.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unified: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionReq {
    pub call_id: ToolCallId,
    pub tool: String,
    pub reason: String,
    pub taint_forced: bool,
    pub input_summary: String,
    pub input: serde_json::Value,
    pub mode: String,
}
