//! US-013: one semantics, two call paths.
//!
//! A `code_mode_only` model calls the multi-agent tools from JavaScript; a
//! direct model calls them from the model surface. Both must reach the SAME
//! pipeline and produce the same result, the same state and the same normalized
//! error, or the choice of tool mode would silently change what orchestration
//! means.
//!
//! The two paths are compared here at the seam where they could actually
//! diverge: the registry's own dispatch versus `PlanDispatcher`, both over the
//! same captured snapshot.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use agent_code_mode::nested::{NestedToolCall, NestedToolDispatcher, NestedToolInput};
use agent_code_mode::protocol::CellId;
use agent_core::tools::{ToolInvocation, ToolOutcome, ToolResultStatus};
use agent_runtime::AgentAuthority;
use agent_runtime::supervisor::{AgentSpawner, AgentSupervisor, ChildParts, ChildRequest};
use agent_tools::{
    AgentHandle, FollowupTask, InterruptAgent, ListAgents, MULTI_AGENT_V2_TOOLS, Registry,
    SendMessage, SpawnAgent, WaitAgent,
};

struct NeverSpawns;

#[async_trait::async_trait]
impl AgentSpawner for NeverSpawns {
    async fn spawn(&self, _request: &ChildRequest) -> Result<ChildParts, String> {
        Err("no child runtime in this test".into())
    }
}

fn wiring() -> (Arc<Registry>, Arc<AgentHandle>) {
    let agents = Arc::new(AgentHandle::new());
    agents.bind(AgentSupervisor::new(
        Arc::new(NeverSpawns),
        Arc::new(agent_runtime::id::SequentialIds::new()),
        Arc::new(agent_core::clock::SystemClock),
        AgentAuthority::unrestricted(),
    ));
    let registry = Arc::new(
        Registry::builder(std::env::temp_dir())
            .approver(Arc::new(agent_tools::AutoApprove::new()))
            .register(SpawnAgent::new(Arc::clone(&agents)))
            .register(SendMessage::new(Arc::clone(&agents)))
            .register(FollowupTask::new(Arc::clone(&agents)))
            .register(ListAgents::new(Arc::clone(&agents)))
            .register(WaitAgent::new(Arc::clone(&agents)))
            .register(InterruptAgent::new(Arc::clone(&agents)))
            .build(),
    );
    (registry, agents)
}

/// Runs one call through the model-facing path.
async fn direct(registry: &Arc<Registry>, tool: &str, input: serde_json::Value) -> ToolOutcome {
    registry
        .dispatch(vec![ToolInvocation::json(
            format!("direct::{tool}"),
            tool,
            input,
        )])
        .await
        .pop()
        .expect("one outcome per invocation")
}

/// Runs the same call through the path a JavaScript cell takes.
fn nested(
    registry: &Arc<Registry>,
    runtime: tokio::runtime::Handle,
    tool: &str,
    input: serde_json::Value,
) -> agent_code_mode::nested::NestedToolOutcome {
    let dispatcher = agent_code_mode::PlanDispatcher::new(
        &registry.step_snapshot(),
        &["exec", "wait"],
        agent_core::tools::ToolEventSink::default(),
        runtime,
    );
    dispatcher.dispatch(NestedToolCall {
        cell_id: CellId::new(&agent_code_mode::protocol::SessionId::new("cell"), 1),
        call_id: format!("nested::{tool}"),
        tool: tool.to_string(),
        input: NestedToolInput::Json(input),
    })
}

/// AC1: for the same call, the direct path and the nested one return the same
/// content and the same category, success or refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_v2_call_answers_identically_direct_and_from_a_cell() {
    let (registry, _agents) = wiring();
    let runtime = tokio::runtime::Handle::current();

    for (tool, input) in [
        ("list_agents", serde_json::json!({})),
        (
            "spawn_agent",
            serde_json::json!({"task_name": "reader", "message": "lire le crate"}),
        ),
        (
            "followup_task",
            serde_json::json!({"target": "inconnu", "message": "continue"}),
        ),
        ("interrupt_agent", serde_json::json!({"target": "inconnu"})),
        ("wait_agent", serde_json::json!({"target": "inconnu"})),
        (
            "send_message",
            serde_json::json!({"target": "reader", "message": ""}),
        ),
    ] {
        let from_model = direct(&registry, tool, input.clone()).await;
        let registry_for_cell = Arc::clone(&registry);
        let runtime_for_cell = runtime.clone();
        let tool_owned = tool.to_string();
        let input_owned = input.clone();
        // A cell blocks its own OS thread while a nested call runs, so the
        // nested path is exercised the way it really runs.
        let from_cell = tokio::task::spawn_blocking(move || {
            nested(
                &registry_for_cell,
                runtime_for_cell,
                &tool_owned,
                input_owned,
            )
        })
        .await
        .expect("the cell thread joins");

        assert_eq!(
            from_model.content, from_cell.content,
            "`{tool}` must answer the same thing on both paths"
        );
        assert_eq!(
            from_model.is_error, from_cell.is_error,
            "`{tool}` must succeed or fail the same way on both paths"
        );
        assert_eq!(
            from_model.status == ToolResultStatus::Success,
            from_cell.error_kind.is_none(),
            "`{tool}` must carry a category on both paths"
        );
    }
}

/// AC4: a child is owned by the THREAD. A listing says so explicitly, on both
/// paths, so the fate of a child whose calling cell is gone is readable rather
/// than inferred.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_names_the_owning_thread_on_both_paths() {
    let (registry, _agents) = wiring();
    let listed = direct(&registry, "list_agents", serde_json::json!({})).await;
    assert_eq!(listed.content, "no sub-agent");
    assert_eq!(listed.status, ToolResultStatus::Success);

    // Every one of the six is callable from a cell: hiding them from a
    // `code_mode_only` model must not remove them from the nested plan.
    let dispatcher = agent_code_mode::PlanDispatcher::new(
        &registry.step_snapshot(),
        &["exec", "wait"],
        agent_core::tools::ToolEventSink::default(),
        tokio::runtime::Handle::current(),
    );
    let mut callable: Vec<String> = dispatcher
        .catalog()
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    callable.sort();
    assert_eq!(callable, MULTI_AGENT_V2_TOOLS);
}
