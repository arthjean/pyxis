//! `AgentEvent` — LE contrat cœur → clients (TUI, `-p` headless, Paneflow).
//! Structuré, sérialisable, AUCUNE décision de présentation, JAMAIS d'ANSI
//! (ARCHITECTURE §10.1, invariant 2). Distinct de `StreamEvent` (provider→cœur).

use crate::compaction::CompactKind;
use crate::error::AgentError;
use crate::message::ToolCallId;
use crate::transition::ExhaustReason;

#[derive(Debug, Clone)]
pub enum AgentEvent {
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

#[derive(Debug, Clone)]
pub struct ToolCallView {
    pub id: ToolCallId,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ToolResultView {
    pub id: ToolCallId,
    pub content: String,
    pub is_error: bool,
    /// Sortie d'outil = untrusted par défaut (taint, US-013).
    pub untrusted: bool,
}

#[derive(Debug, Clone)]
pub struct PermissionReq {
    pub tool: String,
    pub reason: String,
}
