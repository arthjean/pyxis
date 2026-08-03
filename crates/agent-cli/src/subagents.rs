//! The binary's half of the sub-agent contract (US-011).
//!
//! `agent-runtime` owns the graph, the leases, the canonical names, the
//! authority intersection and the cancellation tree. What only the binary can
//! hold is the other half: which durable file a child writes to, which provider
//! and tokenizer it runs on, and above all WHICH TOOLS it may call, because a
//! child's authority is enforced at the tool boundary and nowhere else.
//!
//! Three properties are deliberate:
//! - a child's registry is BUILT from its granted authority, tool by tool. A
//!   mutating tool the authority does not allow is not registered at all, so a
//!   refusal cannot depend on a check someone forgets to write;
//! - a child never gets the multi-agent tools. That is what makes the depth
//!   limit structural instead of a rule the graph has to catch after the model
//!   already tried;
//! - a child's approver is [`agent_tools::AutoDeny`]. A sub-agent has no human
//!   in front of it, so anything that would ask is refused rather than silently
//!   granted.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use agent_core::clock::Clock;
use agent_core::message::Message;
use agent_core::provider::Provider;
use agent_core::session::Session;
use agent_core::step::StepContextSource;
use agent_core::{AgentContext, Deps};
use agent_runtime::context::{StepContexts, StepSource, TurnContextSource};
use agent_runtime::id::{IdGenerator, RandomIds};
use agent_runtime::runner::{RunAgentRunner, TurnRequest};
use agent_runtime::store::{MemoryThreadStore, ThreadStore};
use agent_runtime::supervisor::{AgentSpawner, ChildParts, ChildRequest};
use agent_session::JsonlThreadStore;
use tokio_util::sync::CancellationToken;

use crate::runtime::{CliStepSource, SettingsCell, TurnSettings};
use crate::session::SharedSession;

/// Tools a child may run whatever its authority: they mutate nothing. Sorted,
/// because that is how a registry lists them.
#[cfg(test)]
const CHILD_READ_ONLY_TOOLS: [&str; 5] = ["glob", "grep", "read", "update_plan", "view_image"];

/// What a sub-agent is told about its own situation. Appended to the model's
/// own instructions so a child knows it reports back instead of asking.
const CHILD_FRAMING: &str = "\
You are a sub-agent working for another agent. You cannot ask questions: nobody \
will answer. Your context contains only the task you were given, never the parent \
conversation. Work with the tools you have, and finish with a self-contained \
report: what you looked at, what you found, and what you could not establish. \
Your final message is the only thing your parent receives.";

/// Everything a child needs that does not depend on which child it is.
pub struct SubAgentSpawner {
    provider: Arc<dyn Provider>,
    tokenizer: Arc<dyn agent_tokenizer::TokenCounter>,
    clock: Arc<dyn Clock>,
    workspace: PathBuf,
    sandbox: agent_core::sandbox::SandboxPolicy,
    sandbox_enforced: bool,
    hooks: Arc<dyn agent_tools::hooks::Hooks>,
    harden: agent_tools::CommandHardener,
    /// Durable log of the parent. A child's file is written next to it; `None`
    /// for an ephemeral run, where a child persists nothing either.
    parent_log: Option<PathBuf>,
    /// Settings of the parent, bound after the registry exists. A child inherits
    /// the model and effort in force, read at spawn time so a `/models` between
    /// two spawns is honoured.
    settings: OnceLock<Arc<SettingsCell>>,
}

impl SubAgentSpawner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn Provider>,
        tokenizer: Arc<dyn agent_tokenizer::TokenCounter>,
        clock: Arc<dyn Clock>,
        workspace: PathBuf,
        sandbox: agent_core::sandbox::SandboxPolicy,
        sandbox_enforced: bool,
        hooks: Arc<dyn agent_tools::hooks::Hooks>,
        harden: agent_tools::CommandHardener,
        parent_log: Option<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            provider,
            tokenizer,
            clock,
            workspace,
            sandbox,
            sandbox_enforced,
            hooks,
            harden,
            parent_log,
            settings: OnceLock::new(),
        })
    }

    /// Binds the parent's settings. Called once, after the registry the
    /// settings are composed from exists.
    pub fn bind_settings(&self, settings: Arc<SettingsCell>) {
        if self.settings.set(settings).is_err() {
            tracing::warn!(
                "sub-agent spawner already bound to a settings cell; the binding is kept"
            );
        }
    }

    /// Settings of a child: the parent's model and effort, its own workspace,
    /// and NO session goal. A goal is the user's instruction to the
    /// conversation, not a brief for a child that never saw it.
    fn child_settings(&self, authority: &agent_runtime::AgentAuthority) -> TurnSettings {
        let parent = self
            .settings
            .get()
            .map(|settings| settings.read(TurnSettings::clone));
        match parent {
            Some(mut settings) => {
                settings.goal = None;
                settings.permission_mode = "default (sub-agent: no approver)".into();
                settings.workspace = self.workspace.clone();
                settings.tool_guidelines = Vec::new();
                settings.sandbox =
                    format!("{} (sub-agent: {})", settings.sandbox, authority.label());
                settings
            }
            None => TurnSettings {
                model: String::new(),
                reasoning_effort: None,
                tool_guidelines: Vec::new(),
                goal: None,
                run_config: agent_core::RunConfig::default(),
                permission_mode: "default (sub-agent: no approver)".into(),
                sandbox: "sub-agent".into(),
                workspace: self.workspace.clone(),
            },
        }
    }

    /// Registry of one child: the read-only tools, plus exactly the mutating
    /// tools its authority allows. Never the multi-agent tools and never the
    /// Code Mode pair.
    fn child_registry(
        &self,
        authority: &agent_runtime::AgentAuthority,
        context_window: agent_core::budget::ContextWindowState,
    ) -> Arc<agent_tools::Registry> {
        let mut builder = agent_tools::Registry::builder(&self.workspace)
            .mode(agent_tools::PermissionMode::Default)
            // No human in front of a child: anything that would ask is denied.
            .approver(Arc::new(agent_tools::AutoDeny))
            .hooks(Arc::clone(&self.hooks))
            .command_hardener(Arc::clone(&self.harden))
            .vision(self.provider.capabilities().vision)
            .sandbox(self.sandbox.clone(), self.sandbox_enforced)
            // A child runs its own loop, hence its own budget: sharing the
            // parent's handle would make each side report the other's window.
            .context_window(context_window)
            .register(agent_tools::Read)
            .register(agent_tools::Glob)
            .register(agent_tools::Grep)
            .register(agent_tools::ViewImage)
            .register(agent_tools::UpdatePlan)
            .register(agent_tools::GetContextRemaining);
        // One check, one source of truth: the authority decides, tool by tool.
        if authority.allows("write", false) {
            builder = builder.register(agent_tools::Write);
        }
        if authority.allows("edit", false) {
            builder = builder.register(agent_tools::Edit);
        }
        if authority.allows("apply_patch", false) {
            builder = builder.register(agent_tools::ApplyPatch);
        }
        if authority.allows("bash", false) {
            builder = builder.register(agent_tools::Bash);
        }
        Arc::new(builder.build())
    }
}

#[async_trait::async_trait]
impl AgentSpawner for SubAgentSpawner {
    async fn spawn(&self, request: &ChildRequest) -> Result<ChildParts, String> {
        let ids: Arc<dyn IdGenerator> = Arc::new(RandomIds);
        // One handle per child loop, shared with the child's registry below.
        let context_window = agent_core::budget::ContextWindowState::new();
        let registry = self.child_registry(&request.authority, context_window.clone());

        // The child's file sits next to its parent's, named by its own thread
        // identifier. Reopening the SAME path is what makes a restored child
        // resume its own transcript instead of starting a second one.
        let (store, session, conversation): (Arc<dyn ThreadStore>, _, _) = match &self.parent_log {
            Some(parent) => {
                let path = agent_session::branch_path(parent, request.child_thread_id);
                let store = Arc::new(
                    JsonlThreadStore::open(&path).map_err(|err| format!("sub-agent log: {err}"))?,
                );
                let (session, conversation) =
                    SharedSession::over(Arc::clone(&store) as Arc<dyn Session>);
                (store as Arc<dyn ThreadStore>, session, conversation)
            }
            None => {
                let (session, conversation) = SharedSession::ephemeral();
                (
                    Arc::new(MemoryThreadStore::new()) as Arc<dyn ThreadStore>,
                    session,
                    conversation,
                )
            }
        };

        let settings = SettingsCell::with_provider(
            self.child_settings(&request.authority),
            Arc::clone(&self.provider),
        );
        let steps: Arc<dyn StepContextSource> = Arc::new(StepContexts::new(
            CliStepSource::new(Arc::clone(&registry), Vec::new()) as Arc<dyn StepSource>,
            Arc::clone(&ids),
        ));
        let deps = Deps {
            provider: Arc::clone(&self.provider),
            session: Arc::clone(&session) as Arc<dyn Session>,
            tokenizer: Arc::clone(&self.tokenizer),
            clock: Arc::clone(&self.clock),
            tools: Arc::clone(&registry) as Arc<dyn agent_core::tools::ToolDispatch>,
            // Replaced by the turn's own child token in `RunAgentRunner`.
            cancel: CancellationToken::new(),
            context_window,
        };
        let runner = {
            let registry = Arc::clone(&registry);
            Arc::new(RunAgentRunner::new(deps, move |request: &TurnRequest| {
                child_context(&conversation, &steps, &registry, request)
            }))
        };

        Ok(ChildParts {
            store,
            runner,
            turn_contexts: settings as Arc<dyn TurnContextSource>,
        })
    }
}

/// Composes the `AgentContext` of one child turn.
///
/// Same shape as the parent's, minus everything that belongs to the user's
/// conversation: no project context, no injected skill body, no goal.
fn child_context(
    conversation: &Arc<Mutex<Vec<Message>>>,
    steps: &Arc<dyn StepContextSource>,
    registry: &Arc<agent_tools::Registry>,
    request: &TurnRequest,
) -> AgentContext {
    let mut messages = conversation
        .lock()
        .map(|held| held.clone())
        .unwrap_or_default();
    messages.push(Message::user(request.text.clone()));

    let base = request
        .model_runtime
        .as_ref()
        .map(crate::prompt::select_system_prompt)
        .unwrap_or("You are a helpful assistant.");
    let mut config = agent_core::RunConfig {
        max_turns: request.context.limits.max_turns,
        max_output_tokens: request.context.limits.max_output_tokens,
        ..agent_core::RunConfig::default()
    };
    config.overload_fallback_runtime = request.overload_fallback_runtime.clone();

    AgentContext {
        turn_id: None,
        model: request.context.model.clone(),
        model_runtime: request.model_runtime.clone(),
        reasoning_effort: request.context.reasoning_effort.clone(),
        system: Some(format!("{base}\n\n## Sub-agent\n{CHILD_FRAMING}")),
        overload_fallback_system: None,
        messages,
        tools: registry.tool_specs(),
        config,
        context_messages: Vec::new(),
        ephemeral_messages: Vec::new(),
        step_source: Some(Arc::clone(steps)),
        inputs: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_runtime::AgentAuthority;

    /// A provider that answers nothing: what is under test here is the tool
    /// surface a child is built with, not a turn.
    struct InertProvider {
        caps: agent_core::provider::Capabilities,
    }

    #[async_trait::async_trait]
    impl Provider for InertProvider {
        fn kind(&self) -> agent_core::provider::ProviderKind {
            agent_core::provider::ProviderKind::OpenAiChatGpt
        }
        fn capabilities(&self) -> &agent_core::provider::Capabilities {
            &self.caps
        }
        async fn stream(
            &self,
            _req: agent_core::provider::CanonicalRequest,
        ) -> Result<
            futures_util::stream::BoxStream<
                'static,
                Result<agent_core::provider::StreamEvent, agent_core::provider::ProviderError>,
            >,
            agent_core::provider::ProviderError,
        > {
            Err(agent_core::provider::ProviderError::Transport(
                "unused".into(),
            ))
        }
        async fn complete(
            &self,
            _req: agent_core::provider::CanonicalRequest,
        ) -> Result<agent_core::provider::CanonicalResponse, agent_core::provider::ProviderError>
        {
            Err(agent_core::provider::ProviderError::Transport(
                "unused".into(),
            ))
        }
        fn classify_error(
            &self,
            _err: &agent_core::provider::ProviderError,
        ) -> agent_core::provider::ErrorClass {
            agent_core::provider::ErrorClass::InvalidRequest
        }
    }

    fn spawner() -> Arc<SubAgentSpawner> {
        SubAgentSpawner::new(
            Arc::new(InertProvider {
                caps: agent_core::provider::Capabilities::default(),
            }),
            Arc::new(agent_tokenizer::HeuristicCounter),
            Arc::new(agent_core::clock::SystemClock),
            std::env::temp_dir(),
            agent_core::sandbox::SandboxPolicy::ReadOnly {
                network_access: false,
            },
            false,
            Arc::new(agent_tools::hooks::NoHooks),
            Arc::new(|_command: &mut tokio::process::Command| {}),
            None,
        )
    }

    fn tools_of(authority: &AgentAuthority) -> Vec<String> {
        let mut names: Vec<String> = spawner()
            .child_registry(authority, agent_core::budget::ContextWindowState::new())
            .tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        names.sort();
        names
    }

    /// US-011 AC2 / FR-11: a child's registry IS its authority. A read-only
    /// child has no mutating tool to be refused at, and a grant the parent
    /// never held creates nothing.
    #[test]
    fn a_child_registry_contains_exactly_what_its_authority_allows() {
        assert_eq!(
            tools_of(&AgentAuthority::read_only()),
            CHILD_READ_ONLY_TOOLS
        );

        let granted = AgentAuthority::unrestricted().grant(&AgentAuthority::with_tools(["edit"]));
        let with_edit = tools_of(&granted);
        assert!(with_edit.contains(&"edit".to_string()));
        assert!(!with_edit.contains(&"write".to_string()));
        assert!(!with_edit.contains(&"bash".to_string()));

        // A read-only parent cannot produce a child that edits, whatever the
        // spawn asked for.
        let narrowed =
            AgentAuthority::read_only().grant(&AgentAuthority::with_tools(["edit", "bash"]));
        assert_eq!(tools_of(&narrowed), CHILD_READ_ONLY_TOOLS);
    }

    /// US-013 AC3: the depth limit is structural. A child holds no multi-agent
    /// tool at all, so a spawn cycle is impossible before the graph has to
    /// refuse one.
    #[test]
    fn a_child_never_holds_a_multi_agent_or_code_mode_tool() {
        for authority in [
            AgentAuthority::read_only(),
            AgentAuthority::unrestricted(),
            AgentAuthority::with_tools(["bash", "spawn_agent"]),
        ] {
            let names = tools_of(&authority);
            for forbidden in agent_tools::MULTI_AGENT_V2_TOOLS
                .iter()
                .chain(["exec", "wait"].iter())
            {
                assert!(
                    !names.contains(&(*forbidden).to_string()),
                    "a child must not hold `{forbidden}`: {names:?}"
                );
            }
        }
    }
}
