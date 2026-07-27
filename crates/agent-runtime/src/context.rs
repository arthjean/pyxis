//! Turn-scoped and step-scoped context (US-006).
//!
//! Two lifetimes, deliberately separated:
//!
//! - [`TurnContext`] is what the turn is COMMITTED to: model, effort,
//!   permissions, sandbox, workspace and limits. Captured once when the turn
//!   starts and frozen until its terminal state, so a setting changed mid-turn
//!   never makes the transcript describe a turn that did not happen.
//! - [`StepContext`] is what the model SEES for one request: tool catalog,
//!   project context, invoked skills, environment fragments. Rebuilt before
//!   every request, because an MCP server can connect and an `AGENTS.md` can be
//!   written while the turn runs.
//!
//! The core knows neither: it consumes `agent_core::StepFrame` and never reads a
//! file, a registry or a catalog itself.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_core::message::Message;
use agent_core::provider::ToolSpec;
use agent_core::step::{StepContextSource, StepFrame};
use serde::{Deserialize, Serialize};

use crate::id::{IdGenerator, StepId, TurnId};

/// Byte budget of ONE context section (US-006 AC5). Same order of magnitude as
/// the `AGENTS.md` budget the CLI already applies, so a project file that used
/// to fit still fits.
pub const MAX_SECTION_BYTES: usize = 32 * 1024;
/// Byte budget of every section injected into a single step (US-006 AC5).
pub const MAX_STEP_CONTEXT_BYTES: usize = 64 * 1024;

/// Limits a turn runs under. Constants of the runtime, never configuration
/// (FR-20).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnLimits {
    pub max_turns: u32,
    pub max_output_tokens: u32,
    /// Steering inputs this turn may hold at once.
    pub max_pending_inputs: usize,
}

/// Configuration a turn is committed to until its terminal state (AC1).
///
/// Plain data on purpose: the runtime RECORDS what the turn was started with, it
/// does not own permissions or the sandbox. `permission_mode` and `sandbox` are
/// the labels the authoritative crates expose, kept here so a transcript can be
/// audited without replaying the tool pipeline.
///
/// Durable: it travels with the transition that starts the turn, so a thread
/// resumed after a restart still knows what its last turn ran under (US-009
/// AC1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContext {
    pub turn_id: TurnId,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub permission_mode: String,
    pub sandbox: String,
    pub workspace: PathBuf,
    pub limits: TurnLimits,
}

/// Captures the turn configuration ONCE, when the turn starts.
///
/// A source is free to change between two turns; what it must not do is change
/// the turn already running, which is why the actor calls this exactly once per
/// turn and hands the result to the engine.
pub trait TurnContextSource: Send + Sync {
    fn capture(&self, turn_id: TurnId) -> TurnContext;
}

/// Hands the SAME configuration to every turn, with only the `TurnId`
/// substituted.
///
/// What a one-shot headless run needs, and what makes AC1 observable: change the
/// template while a turn runs and that turn keeps the capture it started with.
pub struct FixedTurnContext {
    template: Mutex<TurnContext>,
}

impl FixedTurnContext {
    pub fn new(template: TurnContext) -> Self {
        Self {
            template: Mutex::new(template),
        }
    }

    /// Replaces the configuration handed to the NEXT turn.
    pub fn set(&self, template: TurnContext) {
        *self
            .template
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = template;
    }
}

impl TurnContextSource for FixedTurnContext {
    fn capture(&self, turn_id: TurnId) -> TurnContext {
        let mut context = self
            .template
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        context.turn_id = turn_id;
        context
    }
}

/// One ordered section of the model-visible context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepSection {
    pub name: String,
    /// `None` when the source could not produce it this step: unreadable file,
    /// malformed catalog, server that went away. NOT an error: the builder falls
    /// back on the last valid value or omits the section (AC6).
    pub content: Option<String>,
    /// A volatile section changes on its own (the date). Volatile sections are
    /// injected AFTER every stable one, so the cacheable prefix keeps its bytes.
    pub volatile: bool,
}

impl StepSection {
    pub fn stable(name: impl Into<String>, content: Option<String>) -> Self {
        Self {
            name: name.into(),
            content,
            volatile: false,
        }
    }

    pub fn volatile(name: impl Into<String>, content: Option<String>) -> Self {
        Self {
            name: name.into(),
            content,
            volatile: true,
        }
    }
}

/// What a source knows at one instant. Read entirely before a step is built, so
/// a change landing mid-build cannot split a catalog across two generations.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepSnapshot {
    pub tools: Vec<ToolSpec>,
    pub sections: Vec<StepSection>,
}

/// Supplies the raw material of a step. Every read of a file, an MCP catalog or
/// a skill happens in an implementation of this trait, outside `agent-core`.
pub trait StepSource: Send + Sync {
    fn snapshot(&self) -> StepSnapshot;
}

/// The model-visible context actually injected into one model request.
#[derive(Debug, Clone, PartialEq)]
pub struct StepContext {
    pub step_id: StepId,
    /// Bumped only when the injected bytes moved. Two steps sharing a generation
    /// produced the same prefix.
    pub generation: u64,
    pub tools: Vec<ToolSpec>,
    pub context_messages: Vec<Message>,
    /// Bounded notes about what was truncated, reused or omitted. Section names
    /// and byte counts ONLY: no fragment of context ever lands in a diagnostic,
    /// which is what keeps them safe to trace at `warn` (confidentiality NFR).
    pub diagnostics: Vec<String>,
}

/// Builds a [`StepContext`] per model request and exposes it to the engine as an
/// `agent_core::StepContextSource`.
pub struct StepContexts {
    source: Arc<dyn StepSource>,
    ids: Arc<dyn IdGenerator>,
    state: Mutex<StepState>,
}

#[derive(Default)]
struct StepState {
    generation: u64,
    /// Last content a section produced successfully, by name. This is what makes
    /// a source that goes unreadable for one step a no-op rather than a hole.
    last_valid: HashMap<String, String>,
    last: Option<StepContext>,
}

impl StepContexts {
    pub fn new(source: Arc<dyn StepSource>, ids: Arc<dyn IdGenerator>) -> Self {
        Self {
            source,
            ids,
            state: Mutex::new(StepState::default()),
        }
    }

    /// Last context handed to the engine. `None` before the first request.
    pub fn last(&self) -> Option<StepContext> {
        self.lock().last.clone()
    }

    /// A poisoned lock is recovered rather than propagated: losing the memo of
    /// the last valid sections would degrade the next step, not corrupt it, and
    /// the runtime must not panic on a step boundary.
    fn lock(&self) -> std::sync::MutexGuard<'_, StepState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Builds the next context: resolve, bound, order, then decide whether the
    /// generation moved.
    pub fn build(&self) -> StepContext {
        let snapshot = self.source.snapshot();
        let step_id = StepId::generate(self.ids.as_ref());
        let mut state = self.lock();
        let mut diagnostics = Vec::new();

        // 1. Resolve each section against the memo, and bound it on its own so
        //    no single injected item can exceed the per-section budget.
        let mut stable: Vec<(String, String)> = Vec::new();
        let mut volatile: Vec<(String, String)> = Vec::new();
        for section in &snapshot.sections {
            let resolved = match &section.content {
                Some(raw) => {
                    let (text, dropped) = truncate_bytes(raw, MAX_SECTION_BYTES);
                    if dropped > 0 {
                        diagnostics.push(format!(
                            "section `{}` truncated to {MAX_SECTION_BYTES} bytes ({dropped} dropped)",
                            section.name
                        ));
                    }
                    state.last_valid.insert(section.name.clone(), text.clone());
                    Some(text)
                }
                None => match state.last_valid.get(&section.name) {
                    Some(previous) => {
                        diagnostics.push(format!("section `{}` unreadable, reused", section.name));
                        Some(previous.clone())
                    }
                    None => {
                        diagnostics.push(format!("section `{}` unreadable, omitted", section.name));
                        None
                    }
                },
            };
            let Some(text) = resolved else { continue };
            if section.volatile {
                volatile.push((section.name.clone(), text));
            } else {
                stable.push((section.name.clone(), text));
            }
        }

        // 2. Stable sections first, volatile last: the prefix a provider can
        //    cache must not move because the date did.
        let mut total = 0usize;
        let mut context_messages = Vec::with_capacity(stable.len() + volatile.len());
        for (name, text) in stable.into_iter().chain(volatile) {
            if total.saturating_add(text.len()) > MAX_STEP_CONTEXT_BYTES {
                diagnostics.push(format!(
                    "section `{name}` dropped, step context budget of {MAX_STEP_CONTEXT_BYTES} bytes reached"
                ));
                continue;
            }
            total += text.len();
            context_messages.push(Message::user(text));
        }

        // 3. The generation moves only when the injected bytes moved. This is
        //    what gives two steps without a source change the same prefix, and
        //    what makes a staged MCP catalog observable at the NEXT step.
        let moved = state.last.as_ref().is_none_or(|last| {
            last.tools != snapshot.tools || last.context_messages != context_messages
        });
        if moved {
            state.generation = state.generation.saturating_add(1);
        }

        for note in &diagnostics {
            tracing::warn!(target: "pyxis::runtime", step_id = %step_id, "{note}");
        }

        let context = StepContext {
            step_id,
            generation: state.generation,
            tools: snapshot.tools,
            context_messages,
            diagnostics,
        };
        state.last = Some(context.clone());
        context
    }
}

impl StepContextSource for StepContexts {
    fn next_frame(&self) -> StepFrame {
        let context = self.build();
        StepFrame {
            generation: context.generation,
            tools: context.tools,
            context_messages: context.context_messages,
        }
    }
}

/// Truncates `text` to at most `cap` BYTES on a char boundary, and reports how
/// many bytes were dropped. Byte-based because the budget bounds the prompt, not
/// a character count.
fn truncate_bytes(text: &str, cap: usize) -> (String, usize) {
    if text.len() <= cap {
        return (text.to_string(), 0);
    }
    let mut cut = cap;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (text[..cut].to_string(), text.len() - cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::SequentialIds;

    struct Fixed(Mutex<StepSnapshot>);

    impl Fixed {
        fn new(snapshot: StepSnapshot) -> Arc<Self> {
            Arc::new(Self(Mutex::new(snapshot)))
        }
        fn set(&self, snapshot: StepSnapshot) {
            *self.0.lock().unwrap() = snapshot;
        }
    }

    impl StepSource for Fixed {
        fn snapshot(&self) -> StepSnapshot {
            self.0.lock().unwrap().clone()
        }
    }

    fn builder(source: Arc<dyn StepSource>) -> StepContexts {
        StepContexts::new(source, Arc::new(SequentialIds::new()))
    }

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "t".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn two_steps_without_a_source_change_produce_the_same_bytes() {
        let source = Fixed::new(StepSnapshot {
            tools: vec![tool("read")],
            sections: vec![
                StepSection::stable("agents", Some("regles".into())),
                StepSection::volatile("environment", Some("<cwd>/tmp</cwd>".into())),
            ],
        });
        let builder = builder(source);

        let first = builder.build();
        let second = builder.build();

        assert_eq!(first.generation, second.generation, "same generation");
        assert_eq!(first.context_messages, second.context_messages);
        assert_ne!(
            first.step_id, second.step_id,
            "each step keeps its identity"
        );
    }

    #[test]
    fn stable_sections_are_injected_before_volatile_ones() {
        let source = Fixed::new(StepSnapshot {
            tools: Vec::new(),
            sections: vec![
                StepSection::volatile("environment", Some("volatile".into())),
                StepSection::stable("agents", Some("stable".into())),
            ],
        });
        let context = builder(source).build();
        let texts: Vec<String> = context.context_messages.iter().map(|m| m.text()).collect();
        assert_eq!(texts, vec!["stable".to_string(), "volatile".to_string()]);
    }

    #[test]
    fn a_staged_catalog_moves_the_generation() {
        let source = Fixed::new(StepSnapshot {
            tools: vec![tool("read")],
            sections: Vec::new(),
        });
        let builder = builder(Arc::clone(&source) as Arc<dyn StepSource>);
        let first = builder.build();

        source.set(StepSnapshot {
            tools: vec![tool("read"), tool("mcp__search")],
            sections: Vec::new(),
        });
        let second = builder.build();

        assert_eq!(second.generation, first.generation + 1);
        assert_eq!(second.tools.len(), 2);
    }

    #[test]
    fn an_oversized_section_is_bounded_with_a_diagnostic() {
        let huge = "a".repeat(MAX_SECTION_BYTES + 4_096);
        let source = Fixed::new(StepSnapshot {
            tools: Vec::new(),
            sections: vec![StepSection::stable("agents", Some(huge))],
        });
        let context = builder(source).build();

        assert_eq!(context.context_messages.len(), 1);
        assert_eq!(context.context_messages[0].text().len(), MAX_SECTION_BYTES);
        assert!(
            context.diagnostics.iter().any(|d| d.contains("truncated")),
            "{:?}",
            context.diagnostics
        );
    }

    #[test]
    fn the_aggregate_budget_drops_the_overflowing_section() {
        let block = "b".repeat(MAX_SECTION_BYTES);
        let source = Fixed::new(StepSnapshot {
            tools: Vec::new(),
            sections: vec![
                StepSection::stable("one", Some(block.clone())),
                StepSection::stable("two", Some(block.clone())),
                StepSection::stable("three", Some(block)),
            ],
        });
        let context = builder(source).build();

        assert_eq!(
            context.context_messages.len(),
            2,
            "64 KiB fits two sections"
        );
        let total: usize = context
            .context_messages
            .iter()
            .map(|m| m.text().len())
            .sum();
        assert!(total <= MAX_STEP_CONTEXT_BYTES);
        assert!(
            context
                .diagnostics
                .iter()
                .any(|d| d.contains("three") && d.contains("budget")),
            "{:?}",
            context.diagnostics
        );
    }

    #[test]
    fn an_unreadable_section_keeps_its_last_valid_value() {
        let source = Fixed::new(StepSnapshot {
            tools: Vec::new(),
            sections: vec![StepSection::stable("agents", Some("v1".into()))],
        });
        let builder = builder(Arc::clone(&source) as Arc<dyn StepSource>);
        let first = builder.build();

        source.set(StepSnapshot {
            tools: Vec::new(),
            sections: vec![StepSection::stable("agents", None)],
        });
        let second = builder.build();

        assert_eq!(second.context_messages, first.context_messages);
        assert_eq!(
            second.generation, first.generation,
            "reusing the last valid value is not a change"
        );
        assert!(second.diagnostics.iter().any(|d| d.contains("reused")));
    }

    #[test]
    fn a_section_unreadable_from_the_start_is_omitted_not_fatal() {
        let source = Fixed::new(StepSnapshot {
            tools: Vec::new(),
            sections: vec![
                StepSection::stable("agents", None),
                StepSection::volatile("environment", Some("env".into())),
            ],
        });
        let context = builder(source).build();

        assert_eq!(context.context_messages.len(), 1);
        assert_eq!(context.context_messages[0].text(), "env");
        assert!(context.diagnostics.iter().any(|d| d.contains("omitted")));
    }

    #[test]
    fn truncation_never_splits_a_char() {
        let text = "é".repeat(10);
        let (cut, dropped) = truncate_bytes(&text, 5);
        assert_eq!(cut, "éé");
        assert_eq!(dropped, text.len() - cut.len());
    }
}
