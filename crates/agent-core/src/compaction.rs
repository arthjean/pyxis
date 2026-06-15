//! Compaction en cascade (ARCHITECTURE §5) : micro → auto/reactive.
//!
//! - **micro** (pur, structurel) : élague le contenu des plus vieux `tool_result`
//!   (les plus volumineux, les moins utiles rétroactivement), garde les récents.
//! - **auto** (proactif) / **reactive** (413/PTL via withholding) : résumé total
//!   via le provider, **images strippées** (on ne re-paye pas la vision).
//! - **circuit breaker** : coupe après N échecs d'autocompact consécutifs au lieu
//!   de boucler (anti error-loop).

use serde::{Deserialize, Serialize};

use crate::error::AgentError;
use crate::message::{ContentBlock, Message, Role};
use crate::provider::{CanonicalRequest, Provider};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactKind {
    Micro,
    Auto,
    Reactive,
}

const PRUNED_PLACEHOLDER: &str = "[résultat d'outil élagué pour économiser le contexte]";

const SUMMARY_SYSTEM: &str = "Tu résumes une conversation entre un utilisateur et un agent de codage. \
Produis un résumé dense et fidèle : objectifs, décisions, fichiers/commandes clés, état courant et \
prochaine étape. Garde tout ce qui est nécessaire pour CONTINUER la tâche sans le contexte original.";

/// État du circuit breaker d'autocompaction.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompactionState {
    consecutive_failures: u32,
}

impl CompactionState {
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
    }
    /// Incrémente et retourne le nouveau compteur d'échecs consécutifs.
    pub fn record_failure(&mut self) -> u32 {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.consecutive_failures
    }
    pub fn tripped(&self, limit: u32) -> bool {
        self.consecutive_failures >= limit
    }
}

/// Microcompact PUR : tronque le contenu des `tool_result` les plus anciens,
/// en gardant intacts les `keep_recent` derniers. Retourne le nombre de blocs
/// élagués. N'altère jamais la structure user/assistant/tool (préserve les
/// `tool_use` correspondants).
pub fn microcompact(messages: &mut [Message], keep_recent: usize) -> usize {
    let tr_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.is_tool_result())
        .map(|(i, _)| i)
        .collect();
    if tr_indices.len() <= keep_recent {
        return 0;
    }
    let cutoff = tr_indices.len() - keep_recent;
    let mut pruned = 0;
    for &i in &tr_indices[..cutoff] {
        for b in &mut messages[i].content {
            if let ContentBlock::ToolResult { content, .. } = b
                && content != PRUNED_PLACEHOLDER
            {
                *content = PRUNED_PLACEHOLDER.to_string();
                pruned += 1;
            }
        }
    }
    pruned
}

/// Compaction `full` (auto / reactive) : strippe les images, demande un résumé
/// total au provider, remplace le transcript par `[résumé] + dernier message
/// utilisateur` (pour conserver l'ask courant). Faillible (l'appel provider peut
/// échouer → circuit breaker côté appelant).
pub async fn full_compact(
    messages: &mut Vec<Message>,
    model: &str,
    provider: &dyn Provider,
) -> Result<(), AgentError> {
    // images strippées AVANT le résumé (on ne re-paye pas les tokens vision).
    // Idempotent : sans danger même si la compaction échoue ensuite.
    for m in messages.iter_mut() {
        m.strip_images();
    }

    // On conserve le dernier message utilisateur (l'ask courant) hors résumé.
    // IMPORTANT : on ne mute PAS `messages` de façon destructive avant que le
    // résumé ait réussi — un échec provider doit préserver le transcript
    // (sinon une compaction ratée détruit la conversation et fausse le circuit
    // breaker).
    let trailing_is_user = matches!(messages.last(), Some(m) if m.role == Role::User);
    let upto = if trailing_is_user {
        messages.len().saturating_sub(1)
    } else {
        messages.len()
    };

    // Rien à résumer (transcript = un seul message user) : ne PAS appeler le
    // provider avec un historique vide. On signale l'impossibilité de compacter
    // (le circuit breaker s'en chargera) plutôt que de détruire le transcript.
    if upto == 0 {
        return Err(AgentError::Compaction(
            "aucun historique à résumer (transcript trop court)".to_string(),
        ));
    }

    let req = CanonicalRequest {
        model: model.to_string(),
        system: Some(SUMMARY_SYSTEM.to_string()),
        messages: messages[..upto].to_vec(),
        tools: Vec::new(),
        max_output_tokens: 1024,
    };
    // `?` ici laisse `messages` intact en cas d'échec (From<ProviderError>).
    let resp = provider.complete(req).await?;
    let summary: String = resp
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    // Résumé vide (refus silencieux, réponse sans bloc Text) : NE PAS écraser le
    // transcript par un contexte vide. `messages` est encore intact ici.
    if summary.trim().is_empty() {
        return Err(AgentError::Compaction(
            "résumé vide reçu du provider".to_string(),
        ));
    }

    let trailing_user = if trailing_is_user {
        messages.last().cloned()
    } else {
        None
    };
    messages.clear();
    messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: format!("[Résumé de la conversation précédente]\n{summary}"),
        }],
    });
    if let Some(u) = trailing_user {
        messages.push(u);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        CanonicalRequest, CanonicalResponse, Capabilities, ErrorClass, ProviderError, ProviderKind,
        StopReason, StreamEvent, TokenUsage,
    };
    use futures_util::stream::BoxStream;

    /// Provider stub : `complete` renvoie une réponse figée (pour tester
    /// `full_compact` en isolation).
    struct StubProvider {
        caps: Capabilities,
        response: CanonicalResponse,
    }

    impl StubProvider {
        fn with_summary(text: &str) -> Self {
            Self {
                caps: caps(),
                response: CanonicalResponse {
                    content: if text.is_empty() {
                        vec![]
                    } else {
                        vec![ContentBlock::Text {
                            text: text.to_string(),
                        }]
                    },
                    usage: TokenUsage::default(),
                    stop: StopReason::EndTurn,
                },
            }
        }
    }

    fn caps() -> Capabilities {
        Capabilities {
            vision: false,
            tools: false,
            prompt_caching: false,
            reasoning: false,
            server_side_state: false,
            max_context: 100_000,
        }
    }

    #[async_trait::async_trait]
    impl Provider for StubProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenAiChatGpt
        }
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }
        async fn stream(
            &self,
            _req: CanonicalRequest,
        ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            Ok(Box::pin(futures_util::stream::empty()))
        }
        async fn complete(
            &self,
            _req: CanonicalRequest,
        ) -> Result<CanonicalResponse, ProviderError> {
            Ok(self.response.clone())
        }
        fn classify_error(&self, _err: &ProviderError) -> ErrorClass {
            ErrorClass::Retryable
        }
    }

    // #6 (CRITICAL) : un résumé vide ne doit PAS écraser le transcript.
    #[tokio::test]
    async fn full_compact_rejects_empty_summary_and_preserves_transcript() {
        let provider = StubProvider::with_summary("");
        let mut messages = vec![Message::user("vieux"), Message::assistant_text("réponse")];
        let before = messages.clone();
        let res = full_compact(&mut messages, "m", &provider).await;
        assert!(res.is_err(), "résumé vide doit échouer");
        assert_eq!(messages, before, "transcript préservé en cas d'échec");
    }

    // #5 : rien à résumer (1 seul message user) → Err, pas d'appel destructeur.
    #[tokio::test]
    async fn full_compact_rejects_when_nothing_to_summarize() {
        let provider = StubProvider::with_summary("résumé");
        let mut messages = vec![Message::user("seul message")];
        let before = messages.clone();
        let res = full_compact(&mut messages, "m", &provider).await;
        assert!(res.is_err());
        assert_eq!(messages, before);
    }

    // chemin nominal : résumé non vide → transcript remplacé par [résumé, last_user].
    #[tokio::test]
    async fn full_compact_replaces_with_summary() {
        let provider = StubProvider::with_summary("RÉSUMÉ");
        let mut messages = vec![
            Message::user("q1"),
            Message::assistant_text("a1"),
            Message::user("q2 courant"),
        ];
        full_compact(&mut messages, "m", &provider).await.unwrap();
        assert_eq!(messages.len(), 2, "[résumé] + dernier message user");
        assert!(messages[0].text().contains("RÉSUMÉ"));
        assert_eq!(messages[1].text(), "q2 courant");
    }

    #[test]
    fn microcompact_prunes_old_keeps_recent() {
        let mut msgs = vec![
            Message::user("go"),
            Message::tool_result("c1", "AAAA très long résultat 1", false),
            Message::tool_result("c2", "BBBB très long résultat 2", false),
            Message::tool_result("c3", "CCCC très long résultat 3", false),
        ];
        let pruned = microcompact(&mut msgs, 1);
        assert_eq!(pruned, 2, "élague les 2 plus vieux, garde le dernier");
        // le dernier tool_result reste intact
        assert!(
            msgs[3].text().is_empty()
                || msgs[3].content.iter().any(|b| matches!(
                    b,
                    ContentBlock::ToolResult { content, .. } if content.starts_with("CCCC")
                ))
        );
        // les vieux sont remplacés par le placeholder
        let placeholders = msgs
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b, ContentBlock::ToolResult { content, .. } if content == PRUNED_PLACEHOLDER))
            .count();
        assert_eq!(placeholders, 2);
    }

    #[test]
    fn microcompact_noop_when_few_results() {
        let mut msgs = vec![Message::tool_result("c1", "x", false)];
        assert_eq!(microcompact(&mut msgs, 2), 0);
    }

    #[test]
    fn circuit_breaker_trips_after_limit() {
        let mut s = CompactionState::default();
        assert_eq!(s.record_failure(), 1);
        assert_eq!(s.record_failure(), 2);
        assert!(!s.tripped(3));
        assert_eq!(s.record_failure(), 3);
        assert!(s.tripped(3));
        s.record_success();
        assert!(!s.tripped(3));
    }
}
