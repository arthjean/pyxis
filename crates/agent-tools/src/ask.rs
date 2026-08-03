//! Tools that address the HUMAN rather than the workspace:
//! `request_user_input` and `request_permissions`. Ported from Codex
//! (`codex-rs/core/src/tools/handlers/request_user_input.rs`,
//! `.../request_permissions.rs`).
//!
//! Without them a blocked model has exactly two moves, and both are bad: guess,
//! or stop. What it cannot do is say what it is missing.
//!
//! The two tools differ in how the answer comes back, and that difference is
//! deliberate:
//!
//! - `request_user_input` does NOT block the turn. The question is published to
//!   the client and the user answers through the ordinary input queue, which
//!   enters the transcript at the loop's next safe point (US-007). Codex blocks
//!   the turn on a modal instead; Pyxis already owns an asynchronous steering
//!   channel, and blocking a turn on a dialog would give a second way for a
//!   conversation to hang.
//! - `request_permissions` DOES block, because the answer changes what the next
//!   call is allowed to do, and a widening granted after the fact is worthless.
//!   It goes through the same approver as any confirmation, so no permission
//!   mode short-circuits it.
//!
//! Neither tool can widen anything by itself. `request_permissions` asks a
//! [`PermissionBroker`], and the broker is what holds the perimeter; absent a
//! broker, the request is refused rather than silently granted.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ValidationError};
use crate::permission::{PermCtx, PermissionDecision, PermissionMode};
use crate::tool::{Tool, ToolCtx, ToolOutput};

/// Longest question or option list a single call may carry. A question longer
/// than this is not a question, it is the model writing its report into a
/// dialog.
const MAX_QUESTION_BYTES: usize = 2_000;
const MAX_OPTIONS: usize = 8;
const MAX_OPTION_BYTES: usize = 200;

/// Publishes a message addressed to the user. Same shape as
/// `agent_sandbox::ProxyNotice`, and injected by the same binary: a question and
/// a blocked host are both things the human has to see, and neither belongs in
/// the model's own output.
pub type UserNotice = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestUserInputInput {
    /// The question, in full. It is shown verbatim.
    pub question: String,
    /// Suggested answers, when the question is a choice. Optional: a free-form
    /// question passes an empty list.
    #[serde(default)]
    pub options: Vec<String>,
}

/// Asks the user a question, without blocking the turn.
pub struct RequestUserInput;

#[async_trait]
impl Tool for RequestUserInput {
    type Input = RequestUserInputInput;

    fn name(&self) -> &str {
        "request_user_input"
    }
    fn description(&self) -> String {
        "Ask the user a question when a missing decision would change what you \
         do, and no reading can settle it. The question is shown immediately; \
         the answer arrives as their next message, so stop and wait for it \
         instead of guessing. Do not use it for questions the workspace can \
         answer, nor to confirm work you were already asked to do."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question, shown verbatim to the user."
                },
                "options": {
                    "type": "array",
                    "description": "Suggested answers, when the question is a \
                                    choice. Empty for a free-form question.",
                    "items": { "type": "string" }
                }
            },
            "required": ["question", "options"],
            "additionalProperties": false
        })
    }
    /// It writes nothing and runs nothing: what it changes is the conversation,
    /// not the machine.
    fn is_read_only(&self) -> bool {
        true
    }
    /// Two questions racing into the same history would interleave, and the user
    /// could not tell which answer belongs to which.
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    /// The question is the model's own text, echoed back to it.
    fn returns_untrusted(&self) -> bool {
        false
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Allow
    }
    fn behavioral_guidelines(&self) -> &[&'static str] {
        ASK_GUIDELINES
    }

    fn validate_input(&self, input: &Self::Input, _ctx: &ToolCtx) -> Result<(), ValidationError> {
        validate_question(&input.question, &input.options)
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let Some(notice) = ctx.user_notice.as_ref() else {
            // Fail-closed on the CLAIM: with no channel to the user, saying the
            // question was asked would make the model wait for an answer that
            // will never come.
            return Ok(ToolOutput::error(
                "no interactive user is attached to this run: the question was \
                 not asked. Decide with what you have, or stop and say what is \
                 missing.",
            ));
        };
        notice(render_question(&input.question, &input.options));
        Ok(ToolOutput::text(format!(
            "The question was shown to the user: {}\nTheir answer will arrive as \
             their next message. Wait for it before acting on it.",
            input.question.trim()
        )))
    }
}

fn render_question(question: &str, options: &[String]) -> String {
    let mut rendered = format!("question: {}", question.trim());
    for (index, option) in options.iter().enumerate() {
        rendered.push_str(&format!("\n  {}. {}", index + 1, option.trim()));
    }
    rendered
}

fn validate_question(question: &str, options: &[String]) -> Result<(), ValidationError> {
    if question.trim().is_empty() {
        return Err(ValidationError::new("question is empty"));
    }
    if question.len() > MAX_QUESTION_BYTES {
        return Err(ValidationError::new(format!(
            "question too large: {} bytes > {MAX_QUESTION_BYTES}",
            question.len()
        )));
    }
    if options.len() > MAX_OPTIONS {
        return Err(ValidationError::new(format!(
            "too many options: {} > {MAX_OPTIONS}",
            options.len()
        )));
    }
    for (index, option) in options.iter().enumerate() {
        if option.trim().is_empty() {
            return Err(ValidationError::new(format!(
                "option {} is empty",
                index + 1
            )));
        }
        if option.len() > MAX_OPTION_BYTES {
            return Err(ValidationError::new(format!(
                "option {} too large: {} bytes > {MAX_OPTION_BYTES}",
                index + 1,
                option.len()
            )));
        }
    }
    Ok(())
}

/// What a permission request can be granted, if a human agrees.
///
/// Deliberately NOT a free-form permission language. Each variant is a perimeter
/// Pyxis can actually widen at runtime; anything else would be a promise the
/// kernel refuses to keep. The filesystem is the case in point: a Landlock
/// domain is inherited and irreversible, so "let me write there" is not a
/// request that can be granted mid-session, and it is not offered.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum PermissionAsk {
    /// Reach a host, and its subdomains, for the rest of the session.
    Network { host: String },
    /// Move the session to a less restrictive permission mode.
    Mode { mode: PermissionMode },
}

/// Outcome of a request. `Refused` carries the reason the model is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantOutcome {
    Granted(String),
    Refused(String),
}

/// Holds the perimeters a request can widen. Implemented by the binary, which
/// is the only place that owns the proxy grants and the permission mode.
///
/// Every method ASKS a human before widening anything: a broker that grants on
/// its own would turn a model request into a privilege escalation.
#[async_trait]
pub trait PermissionBroker: Send + Sync {
    async fn request(&self, ask: &PermissionAsk, reason: &str) -> GrantOutcome;
}

/// No `deny_unknown_fields` here: serde cannot combine it with `flatten`, and
/// the flattened scope is what keeps the wire shape flat for the model. The
/// bound that matters is still enforced, by `validate_input`.
#[derive(Debug, Deserialize)]
pub struct RequestPermissionsInput {
    /// What is being asked for.
    #[serde(flatten)]
    pub ask: PermissionAsk,
    /// Why it is needed. Shown to the user, who is deciding on it.
    pub reason: String,
}

/// Asks for a wider perimeter for the rest of the session.
pub struct RequestPermissions {
    broker: Arc<dyn PermissionBroker>,
}

impl RequestPermissions {
    pub fn new(broker: Arc<dyn PermissionBroker>) -> Self {
        Self { broker }
    }
}

#[async_trait]
impl Tool for RequestPermissions {
    type Input = RequestPermissionsInput;

    fn name(&self) -> &str {
        "request_permissions"
    }
    fn description(&self) -> String {
        "Ask the user to widen what this session may do, when a refusal is \
         blocking the task. Two scopes exist: `network` (reach a host and its \
         subdomains) and `mode` (a less restrictive permission mode). Filesystem \
         confinement cannot be widened while the session runs. Give a concrete \
         reason: the user decides on it."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["network", "mode"],
                    "description": "What is being asked for."
                },
                "host": {
                    "type": ["string", "null"],
                    "description": "Host to reach, for scope=network."
                },
                "mode": {
                    "type": ["string", "null"],
                    "enum": ["accept-edits", "auto", null],
                    "description": "Requested permission mode, for scope=mode. \
                                    `accept-edits` stops asking for file edits; \
                                    `auto` stops asking at all."
                },
                "reason": {
                    "type": "string",
                    "description": "Why the current perimeter blocks the task."
                }
            },
            "required": ["scope", "host", "mode", "reason"],
            "additionalProperties": false
        })
    }
    /// Nothing is written, but a granted request changes what every later call
    /// may do: not read-only, hence refused in `Plan` mode and never parallel.
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    /// The definition of a sensitive action: recent untrusted content asking for
    /// a wider perimeter is the exact shape of an injection payoff.
    fn is_sensitive(&self) -> bool {
        true
    }
    fn returns_untrusted(&self) -> bool {
        false
    }
    /// `Ask` even here: the broker asks again for the widening itself, and this
    /// first question is the one the permission mode and the taint rule shape.
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Ask
    }
    fn behavioral_guidelines(&self) -> &[&'static str] {
        ASK_GUIDELINES
    }

    fn validate_input(&self, input: &Self::Input, _ctx: &ToolCtx) -> Result<(), ValidationError> {
        if input.reason.trim().is_empty() {
            return Err(ValidationError::new(
                "reason is empty: the user decides on it",
            ));
        }
        match &input.ask {
            PermissionAsk::Network { host } if host.trim().is_empty() => {
                Err(ValidationError::new("host is empty"))
            }
            // `full-access` short-circuits every check, including the injection
            // defense. It is a decision a human takes on their own terms, never
            // one a model asks for, so the request is refused HERE: before the
            // permission stage, hence unreachable by any mode. `read-only` is
            // refused for the mirror reason: a model must not be able to talk
            // itself out of confirmations by first narrowing, then widening.
            PermissionAsk::Mode { mode }
                if matches!(
                    mode,
                    PermissionMode::BypassPermissions | PermissionMode::Plan
                ) =>
            {
                Err(ValidationError::new(format!(
                    "`{}` cannot be requested by a model; ask for `accept-edits` \
                     or `auto`, or let the user change it themselves",
                    mode.id()
                )))
            }
            // A request for a mode no wider than the current one is not refused
            // here: the broker compares against the mode in force, which this
            // tool deliberately does not read.
            _ => Ok(()),
        }
    }

    async fn call(&self, input: Self::Input, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        match self.broker.request(&input.ask, input.reason.trim()).await {
            GrantOutcome::Granted(message) => Ok(ToolOutput::text(message)),
            // A refusal is a RESULT, not a pipeline failure: the model reads why
            // and can take another route in the same turn.
            GrantOutcome::Refused(reason) => Ok(ToolOutput::error(reason)),
        }
    }
}

const ASK_GUIDELINES: &[&str] = &[
    "request_user_input: ask only when a missing decision changes what you do \
     and no file can settle it. After asking, stop and wait for the answer.",
    "request_permissions: ask when a refusal blocks the task, with a concrete \
     reason. Filesystem confinement cannot be widened mid-session; do not ask.",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn a_malformed_question_is_refused_before_anything_is_shown() {
        assert!(validate_question("  ", &[]).is_err());
        assert!(validate_question("ok?", &["  ".to_string()]).is_err());
        let many: Vec<String> = (0..MAX_OPTIONS + 1).map(|i| i.to_string()).collect();
        assert!(validate_question("ok?", &many).is_err());
        assert!(validate_question("ok?", &["a".to_string()]).is_ok());
    }

    #[tokio::test]
    async fn without_a_channel_the_question_is_reported_as_not_asked() {
        // The failure mode this guards against: a model told "asked" waits for
        // an answer nobody will ever send.
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = RequestUserInput
            .call(
                RequestUserInputInput {
                    question: "ship it?".to_string(),
                    options: Vec::new(),
                },
                &ctx,
            )
            .await
            .expect("a missing channel is a result, not a pipeline error");
        assert!(out.is_error);
        assert!(out.content.contains("not asked"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_question_reaches_the_user_channel_with_its_options() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let ctx = ToolCtx::new(std::env::temp_dir())
            .with_user_notice(Arc::new(move |message: String| {
                sink.lock().expect("test lock").push(message);
            }));
        let out = RequestUserInput
            .call(
                RequestUserInputInput {
                    question: "which backend?".to_string(),
                    options: vec!["postgres".to_string(), "sqlite".to_string()],
                },
                &ctx,
            )
            .await
            .expect("a well-formed question must succeed");
        assert!(!out.is_error);
        let shown = seen.lock().expect("test lock").join("\n");
        assert!(shown.contains("which backend?"), "{shown}");
        assert!(shown.contains("1. postgres"), "{shown}");
        assert!(
            out.content.contains("Wait for it"),
            "the model must be told to wait: {}",
            out.content
        );
    }

    #[test]
    fn the_two_scopes_parse_from_a_flat_wire_shape() {
        let network: RequestPermissionsInput = serde_json::from_value(serde_json::json!({
            "scope": "network",
            "host": "crates.io",
            "reason": "fetch the index"
        }))
        .expect("the network scope must parse");
        assert_eq!(
            network.ask,
            PermissionAsk::Network {
                host: "crates.io".to_string()
            }
        );

        let mode: RequestPermissionsInput = serde_json::from_value(serde_json::json!({
            "scope": "mode",
            "mode": "accept-edits",
            "reason": "twenty files to edit"
        }))
        .expect("the mode scope must parse");
        assert_eq!(
            mode.ask,
            PermissionAsk::Mode {
                mode: PermissionMode::AcceptEdits
            }
        );
    }

    #[tokio::test]
    async fn a_refused_request_comes_back_as_a_readable_error_not_a_failure() {
        struct Refuses;
        #[async_trait]
        impl PermissionBroker for Refuses {
            async fn request(&self, _ask: &PermissionAsk, _reason: &str) -> GrantOutcome {
                GrantOutcome::Refused("the user declined: use the vendored copy".to_string())
            }
        }
        let ctx = ToolCtx::new(std::env::temp_dir());
        let out = RequestPermissions::new(Arc::new(Refuses))
            .call(
                RequestPermissionsInput {
                    ask: PermissionAsk::Network {
                        host: "crates.io".to_string(),
                    },
                    reason: "fetch the index".to_string(),
                },
                &ctx,
            )
            .await
            .expect("a refusal is a result");
        assert!(out.is_error);
        assert!(out.content.contains("vendored copy"), "{}", out.content);
    }

    #[test]
    fn the_modes_a_model_must_not_ask_for_are_refused_before_the_permission_stage() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        struct Never;
        #[async_trait]
        impl PermissionBroker for Never {
            async fn request(&self, _ask: &PermissionAsk, _reason: &str) -> GrantOutcome {
                unreachable!("validation must refuse before the broker is consulted")
            }
        }
        let tool = RequestPermissions::new(Arc::new(Never));
        for refused in [PermissionMode::BypassPermissions, PermissionMode::Plan] {
            let err = tool
                .validate_input(
                    &RequestPermissionsInput {
                        ask: PermissionAsk::Mode { mode: refused },
                        reason: "faster".to_string(),
                    },
                    &ctx,
                )
                .expect_err("a model must not be able to ask for this mode");
            assert!(err.to_string().contains(refused.id()), "{err}");
        }
        assert!(
            tool.validate_input(
                &RequestPermissionsInput {
                    ask: PermissionAsk::Mode {
                        mode: PermissionMode::AcceptEdits
                    },
                    reason: "twenty files to edit".to_string(),
                },
                &ctx,
            )
            .is_ok()
        );
    }

    #[test]
    fn a_request_without_a_reason_is_refused_before_anyone_is_asked() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        struct Never;
        #[async_trait]
        impl PermissionBroker for Never {
            async fn request(&self, _ask: &PermissionAsk, _reason: &str) -> GrantOutcome {
                unreachable!("validation must refuse before the broker is consulted")
            }
        }
        let tool = RequestPermissions::new(Arc::new(Never));
        let err = tool
            .validate_input(
                &RequestPermissionsInput {
                    ask: PermissionAsk::Network {
                        host: "crates.io".to_string(),
                    },
                    reason: "   ".to_string(),
                },
                &ctx,
            )
            .expect_err("an empty reason must be refused");
        assert!(err.to_string().contains("reason"), "{err}");
    }
}
