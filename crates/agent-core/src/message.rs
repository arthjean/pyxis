//! Types canoniques de message (format Anthropic-like, content blocks — cf.
//! PROVIDERS §1.1). `agent-core` est le crate des types canoniques : tout le
//! système (provider, session, tools) ne connaît que ces types.

use serde::{Deserialize, Serialize};

pub type ToolCallId = String;

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
        #[serde(default)]
        is_error: bool,
    },
    Image {
        media_type: String,
        data: String,
    },
    /// Reasoning item CHIFFRÉ du backend Codex (US-031, replay isolé). Capturé
    /// uniquement derrière le flag `reasoning_replay` (défaut OFF) pour réémission
    /// de la paire `rs`/`fc` ; DROPPÉ à la compaction (contrainte protocole). Le
    /// `encrypted_content` est opaque (jamais loggé/affiché).
    EncryptedReasoning {
        id: String,
        encrypted_content: String,
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
        Self::single(
            Role::Tool,
            ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                is_error,
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
            if let ContentBlock::Text { text } = b {
                out.push_str(text);
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
}
