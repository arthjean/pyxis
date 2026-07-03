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
    /// Résultat d'outil (le taint vit dans le view-model — US-013).
    ToolResult(ToolResultView),
    /// Une compaction vient d'avoir lieu.
    Compacted(CompactKind),
    /// Demande d'autorisation (émis par le pipeline d'outils — US-013, non par
    /// le cœur en EP-002 ; présent pour fixer le contrat).
    PermissionAsk(PermissionReq),
    EndTurn,
    Exhausted(ExhaustReason),
    Error(AgentError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallView {
    pub id: ToolCallId,
    pub name: String,
    pub input: serde_json::Value,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionReq {
    pub call_id: ToolCallId,
    pub tool: String,
    pub reason: String,
    pub input_summary: String,
    pub input: serde_json::Value,
    pub mode: String,
}
