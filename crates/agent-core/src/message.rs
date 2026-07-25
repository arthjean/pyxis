//! Types canoniques de message (format Anthropic-like, content blocks — cf.
//! PROVIDERS §1.1). `agent-core` est le crate des types canoniques : tout le
//! système (provider, session, tools) ne connaît que ces types.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

pub type ToolCallId = String;

/// US-002 — contenu du résultat écrit pour un appel d'outil resté sans réponse
/// après une interruption. Il sert deux publics : le backend, qui rejette en 400
/// tout `tool_use` sans `tool_result` correspondant, et le modèle, qui doit lire
/// dans l'historique que l'utilisateur a interrompu l'exécution.
///
/// Le message porte la CONSÉQUENCE et pas seulement le fait : un outil interrompu
/// peut avoir écrit la moitié de ses effets. Sans cette précision, le modèle
/// suppose par défaut que rien n'a eu lieu et reprend sur un état faux (même
/// raisonnement que le `<turn_aborted>` de Codex CLI).
pub const INTERRUPTED_TOOL_RESULT: &str = "Interrupted by the user before this tool call \
     completed. No result is available: the tool may have partially executed and any process \
     it started may still be running.";

/// Appels d'outils du transcript restés SANS résultat, dans l'ordre d'apparition.
///
/// C'est la définition unique de l'appariement `tool_use` ↔ `tool_result` :
/// la boucle s'en sert pour réconcilier avant persistance (US-002) et l'adapter
/// provider pour refuser d'émettre un appel orphelin (US-003). Un même id
/// n'est rendu qu'une fois : la réparation ne peut pas produire de doublon.
pub fn unanswered_tool_calls(messages: &[Message]) -> Vec<ToolCallId> {
    let mut answered: HashSet<&str> = HashSet::new();
    for block in messages.iter().flat_map(|m| m.content.iter()) {
        if let ContentBlock::ToolResult { tool_use_id, .. } = block {
            answered.insert(tool_use_id.as_str());
        }
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut pending = Vec::new();
    for block in messages.iter().flat_map(|m| m.content.iter()) {
        if let ContentBlock::ToolUse { id, .. } = block
            && !answered.contains(id.as_str())
            && seen.insert(id.as_str())
        {
            pending.push(id.clone());
        }
    }
    pending
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    UnknownTool,
    Parse,
    Validation,
    OutsideWorkspace,
    Io,
    Rejected,
    PermissionDenied,
    Timeout,
    Semantic,
}

const fn default_untrusted() -> bool {
    true
}

const fn default_summary_source_untrusted() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Bloc de contenu canonique (`text` / `thinking` / `tool_use` / `tool_result` /
/// `image`). À la compaction `full`, les blocs `Image` sont strippés (§5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolUse {
        id: ToolCallId,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: ToolCallId,
        content: String,
        /// Résultat d'outil non fiable par défaut. Les anciens JSONL sans ce champ
        /// sont relus en fail-closed.
        #[serde(default = "default_untrusted")]
        untrusted: bool,
        #[serde(default)]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_kind: Option<ToolErrorKind>,
    },
    Image {
        media_type: String,
        data: String,
    },
    /// Reasoning item CHIFFRÉ du backend Codex (US-031, replay isolé). Capturé
    /// quand `reasoning_replay` est actif pour réémission de la paire `rs`/`fc` ;
    /// DROPPÉ à la compaction (contrainte protocole). Le `encrypted_content` est
    /// opaque (jamais loggé/affiché).
    EncryptedReasoning {
        id: String,
        encrypted_content: String,
    },
    /// Résumé de compaction typé. Les anciens logs utilisaient un message user texte
    /// préfixé; ce variant évite les collisions avec un vrai prompt utilisateur.
    Summary {
        text: String,
        /// Vrai si le résumé dérive au moins en partie de sorties d'outils ou de
        /// résumés dont la confiance est inconnue. Les anciens JSONL relus sans ce
        /// champ échouent en sécurité.
        #[serde(default = "default_summary_source_untrusted")]
        source_untrusted: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self::single(Role::System, ContentBlock::Text { text: text.into() })
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self::single(Role::User, ContentBlock::Text { text: text.into() })
    }
    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
        }
    }
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::single(Role::Assistant, ContentBlock::Text { text: text.into() })
    }
    pub fn tool_result(
        id: impl Into<ToolCallId>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self::tool_result_with_trust(id, content, is_error, true)
    }

    pub fn tool_result_with_trust(
        id: impl Into<ToolCallId>,
        content: impl Into<String>,
        is_error: bool,
        untrusted: bool,
    ) -> Self {
        Self::tool_result_with_metadata(id, content, is_error, untrusted, None)
    }

    pub fn tool_result_with_metadata(
        id: impl Into<ToolCallId>,
        content: impl Into<String>,
        is_error: bool,
        untrusted: bool,
        error_kind: Option<ToolErrorKind>,
    ) -> Self {
        Self::single(
            Role::Tool,
            ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                untrusted,
                is_error,
                error_kind,
            },
        )
    }

    fn single(role: Role, block: ContentBlock) -> Self {
        Self {
            role,
            content: vec![block],
        }
    }

    /// Concatène tous les blocs `Text` (utile pour résumés / affichage).
    pub fn text(&self) -> String {
        let mut out = String::new();
        for b in &self.content {
            match b {
                ContentBlock::Text { text } | ContentBlock::Summary { text, .. } => {
                    out.push_str(text);
                }
                _ => {}
            }
        }
        out
    }

    /// Cette message porte-t-elle au moins un `tool_result` ? (cible du
    /// microcompact : on élague les plus vieux en premier.)
    pub fn is_tool_result(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    }

    pub fn has_images(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. }))
    }

    /// Retire les blocs `Image` (compaction full : on ne re-paye pas la vision).
    /// Retourne le nombre de blocs retirés.
    pub fn strip_images(&mut self) -> usize {
        let before = self.content.len();
        self.content
            .retain(|b| !matches!(b, ContentBlock::Image { .. }));
        before - self.content.len()
    }

    /// Le message transporte-t-il du contenu qui doit rester traité comme non fiable
    /// par les prochaines décisions d'outils ou de compaction ?
    pub fn carries_untrusted_content(&self) -> bool {
        self.content.iter().any(ContentBlock::carries_untrusted_content)
            || (self.role == Role::User
                && self
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[Previous conversation summary]\n"))))
    }

    pub fn validate(&self) -> Result<(), MessageValidationError> {
        if self.content.is_empty() {
            return Err(MessageValidationError::EmptyContent);
        }
        for block in &self.content {
            match (self.role, block) {
                (Role::System, ContentBlock::Text { .. }) => {}
                (
                    Role::User,
                    ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::Summary { .. },
                ) => {}
                (
                    Role::Assistant,
                    ContentBlock::Text { .. }
                    | ContentBlock::Thinking { .. }
                    | ContentBlock::ToolUse { .. }
                    | ContentBlock::EncryptedReasoning { .. },
                ) => {}
                (Role::Tool, ContentBlock::ToolResult { .. }) => {}
                _ => {
                    return Err(MessageValidationError::InvalidBlockForRole {
                        role: self.role,
                        block_type: block.kind(),
                    });
                }
            }
        }
        Ok(())
    }
}

impl ContentBlock {
    pub fn kind(&self) -> &'static str {
        match self {
            ContentBlock::Text { .. } => "text",
            ContentBlock::Thinking { .. } => "thinking",
            ContentBlock::ToolUse { .. } => "tool_use",
            ContentBlock::ToolResult { .. } => "tool_result",
            ContentBlock::Image { .. } => "image",
            ContentBlock::EncryptedReasoning { .. } => "encrypted_reasoning",
            ContentBlock::Summary { .. } => "summary",
        }
    }

    pub fn carries_untrusted_content(&self) -> bool {
        match self {
            ContentBlock::ToolResult { untrusted, .. } => *untrusted,
            ContentBlock::Summary {
                source_untrusted, ..
            } => *source_untrusted,
            _ => false,
        }
    }
}

/// Indique si la queue récente du transcript contient encore du contenu non fiable.
/// Utilisé au resume pour re-semer le taint de permission sans scanner tout le log.
pub fn recent_untrusted_content(messages: &[Message], window_messages: usize) -> bool {
    messages
        .iter()
        .rev()
        .take(window_messages)
        .any(Message::carries_untrusted_content)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MessageValidationError {
    #[error("message content is empty")]
    EmptyContent,
    #[error("block {block_type} is invalid for role {role:?}")]
    InvalidBlockForRole {
        role: Role,
        block_type: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_concatenates_text_blocks_only() {
        let m = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "a".into() },
                ContentBlock::Thinking {
                    text: "ignored".into(),
                },
                ContentBlock::Text { text: "b".into() },
            ],
        };
        assert_eq!(m.text(), "ab");
    }

    // US-031 : la variante EncryptedReasoning sérialise en tag snake_case et
    // round-trip (rétro-compat JSONL : variante additive, sessions existantes intactes).
    #[test]
    fn encrypted_reasoning_serde_roundtrip() {
        let b = ContentBlock::EncryptedReasoning {
            id: "rs_1".into(),
            encrypted_content: "OPAQUE".into(),
        };
        let json = serde_json::to_string(&b).unwrap();
        assert!(json.contains("\"type\":\"encrypted_reasoning\""));
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(b, back);
    }

    #[test]
    fn strip_images_removes_image_blocks() {
        let mut m = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text { text: "hi".into() },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "xxxx".into(),
                },
            ],
        };
        assert!(m.has_images());
        assert_eq!(m.strip_images(), 1);
        assert!(!m.has_images());
        assert_eq!(m.content.len(), 1);
    }

    #[test]
    fn tool_result_detection() {
        assert!(Message::tool_result("id1", "out", false).is_tool_result());
        assert!(!Message::user("hi").is_tool_result());
    }

    // US-002/US-003 : l'appariement rend les appels SANS résultat, dans l'ordre,
    // une seule fois chacun — c'est ce qui garantit « ni doublon ni oubli ».
    #[test]
    fn unanswered_tool_calls_reports_each_pending_call_once_in_order() {
        let messages = vec![
            Message::assistant(vec![
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                },
                ContentBlock::ToolUse {
                    id: "c2".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                },
            ]),
            Message::tool_result("c2", "done", false),
            // Répétition défensive du même appel : un seul résultat doit en découler.
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }]),
        ];
        assert_eq!(unanswered_tool_calls(&messages), vec!["c1".to_string()]);
    }

    #[test]
    fn unanswered_tool_calls_is_empty_on_a_healthy_transcript() {
        let messages = vec![
            Message::user("go"),
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }]),
            Message::tool_result("c1", "out", false),
        ];
        assert!(unanswered_tool_calls(&messages).is_empty());
    }

    #[test]
    fn tool_result_untrusted_defaults_to_true_for_old_json() {
        let json = r#"{"type":"tool_result","tool_use_id":"c1","content":"out","is_error":false}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert!(matches!(
            block,
            ContentBlock::ToolResult {
                untrusted: true,
                ..
            }
        ));
    }

    #[test]
    fn summary_source_untrusted_defaults_to_true_for_old_json() {
        let json = r#"{"type":"summary","text":"summary"}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert!(matches!(
            block,
            ContentBlock::Summary {
                source_untrusted: true,
                ..
            }
        ));
    }

    #[test]
    fn recent_untrusted_content_checks_tail() {
        let clean = Message::user("ok");
        let tainted = Message::tool_result("c1", "evil", false);
        assert!(recent_untrusted_content(&[clean.clone(), tainted], 2));
        assert!(!recent_untrusted_content(
            &[
                Message::tool_result_with_trust("c1", "ok", false, false),
                clean
            ],
            1
        ));
    }

    #[test]
    fn validation_rejects_tool_result_in_assistant_message() {
        let msg = Message::assistant(vec![ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            content: "out".into(),
            untrusted: true,
            is_error: false,
            error_kind: None,
        }]);
        assert!(msg.validate().is_err());
    }
}
