//! MCP tools exposed to the model as `DynTool` (US-011).
//!
//! What lives here is **trust**: everything coming from a server is untrusted
//! (CVE-2025-6514), so the metadata is fail-closed, the baseline is `Ask`, an
//! approval is remembered per act rather than per tool name, the taint is
//! propagated in full, and server prose never reaches a tool description.
//!
//! The two mechanical problems that also have to be solved before a tool can be
//! registered live next door, because neither is a trust decision: `naming`
//! (the 64-byte `^[A-Za-z0-9_-]+$` name) and `schema` (the strict form the
//! provider validates).

use std::collections::BTreeSet;
use std::time::Duration;

use agent_core::provider::ToolSpec;
use agent_tools::error::{ToolError, ValidationError};
use agent_tools::permission::{ApprovalMemo, PermCtx, PermissionDecision};
use agent_tools::tool::{DynTool, MAX_TOOL_INPUT_BYTES, ToolCtx, ToolOutput, estimate_json_bytes};
use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::call::McpClient;
use crate::client::McpToolInfo;
use crate::config::{McpApproval, McpServerPolicy, McpToolPolicy};
use crate::naming::qualified_name;
use crate::schema::{MAX_SCHEMA_BYTES, strict_input_schema};

/// Grace added on top of the client bound so OUR error (which names the server)
/// wins over the Registry's generic timeout.
const REGISTRY_TIMEOUT_GRACE: Duration = Duration::from_secs(2);

/// One tool of one server, dispatched like any other tool.
pub struct McpTool {
    /// Name exposed to the model (`mcp__server__tool`, possibly shortened).
    name: String,
    /// Origin server, for the error messages.
    server: String,
    /// Name to call on the server: never the exposed name.
    original_name: String,
    description: String,
    input_schema: Value,
    /// Approval level resolved from the server policy (US-015), already hardened
    /// by the server's own annotation. It is a *baseline*: the permission mode,
    /// the hooks and the taint defense sit above it and can only tighten it.
    approval: McpApproval,
    /// The server declared its calls safe to run beside one another.
    supports_parallel: bool,
    client: McpClient,
}

impl McpTool {
    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn original_name(&self) -> &str {
        &self.original_name
    }
}

#[async_trait]
impl DynTool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> String {
        self.description.clone()
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    /// The server and the tool as the SERVER names it, not the mangled name the
    /// model calls. A client showing `mcp__github__create_issue` shows our
    /// encoding; this shows the fact.
    fn call_kind(&self, _raw: &Value) -> agent_core::event::ToolCallKind {
        agent_core::event::ToolCallKind::Mcp {
            server: self.server.clone(),
            tool: self.original_name.clone(),
        }
    }

    // ───── Fail-closed metadata (invariant 4). Nothing here is *relaxed* by the
    // server's `annotations`: a hint from a remote party may aggravate its own
    // treatment (see `effective_approval`), never soften it.
    /// Serialized by default. Only the human-written per-server
    /// `supportsParallelToolCalls` lifts it, and a workspace file cannot set it.
    fn is_concurrency_safe(&self) -> bool {
        self.supports_parallel
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn is_sensitive(&self) -> bool {
        true
    }

    fn is_taint_sensitive(&self) -> bool {
        true
    }

    /// Always true (ARCHITECTURE 6): a server cannot declare its output trusted.
    fn returns_untrusted(&self) -> bool {
        true
    }

    fn behavioral_guidelines(&self) -> &[&'static str] {
        &[]
    }

    /// MCP tools are what makes a tool list grow without bound: three servers
    /// routinely outweigh the transcript in schemas alone. They are therefore
    /// the deferrable set (ARCHITECTURE 4.5), found back through `tool_search`.
    /// Nothing about dispatch changes: a deferred tool is hidden from the
    /// request, never from the pipeline.
    fn is_deferrable(&self) -> bool {
        true
    }

    /// The server, as the USER named it in their MCP configuration. Grouping by
    /// server is the one grouping that means something here: it is the trust
    /// boundary, the disconnect unit, and what a human reasons about.
    fn namespace(&self) -> Option<&str> {
        Some(&self.server)
    }

    fn precheck(&self, raw: &Value, _ctx: &ToolCtx) -> Result<(), ToolError> {
        let estimated = estimate_json_bytes(raw);
        if estimated > MAX_TOOL_INPUT_BYTES {
            return Err(ToolError::Validation(ValidationError::new(format!(
                "tool input too large: estimated {estimated} bytes > {MAX_TOOL_INPUT_BYTES}"
            ))));
        }
        match raw {
            Value::Object(_) | Value::Null => Ok(()),
            _ => Err(ToolError::Parse(
                "MCP tool arguments must be a JSON object".to_string(),
            )),
        }
    }

    /// A confirmation is requested unless the configuration declared this exact
    /// tool auto-approved (US-015). The shell command classification (US-007) does
    /// not apply here: an MCP call is an opaque action on a remote party, so the
    /// only thing that can lower the baseline is an explicit human decision
    /// written in a file the workspace does not control.
    ///
    /// `Allow` is not the final word: `resolve_permission` upgrades it back to a
    /// confirmation as soon as the turn carries recent untrusted taint, because
    /// `is_taint_sensitive` is true for every MCP tool (US-015 AC3).
    fn permission(&self, _raw: &Value, _ctx: &PermCtx) -> PermissionDecision {
        baseline_permission(self.approval)
    }

    /// The unit of approval is the (server, tool, arguments) triple.
    ///
    /// The arguments belong IN the key. An MCP call has no argv, but its
    /// arguments are its entire security surface: `write_file {path: "notes.md"}`
    /// and `write_file {path: "~/.ssh/authorized_keys"}` are not the same act,
    /// and a key that stops at the tool name would let one answer authorize
    /// both. That is exactly the reasoning that makes `bash` remember an exact
    /// token sequence and never a prefix (CVE-2026-22708); nothing about MCP
    /// weakens it. What the memory buys is still the case that matters: a server
    /// called fifteen times on the same arguments asks once.
    ///
    /// The taint defense sits above this, not under it: `is_taint_sensitive` is
    /// true for every MCP tool, so the Registry refuses to read OR write a
    /// remembered answer while the turn carries recent untrusted content
    /// (US-008). That is a second line. Its window decays over a few dispatch
    /// cycles, so a remembered answer has to be safe on its own.
    fn approval_memo(&self, raw: &Value) -> ApprovalMemo {
        ApprovalMemo::Key(vec![
            self.server.clone(),
            self.original_name.clone(),
            canonical_arguments(raw),
        ])
    }

    fn timeout(&self, _ctx: &ToolCtx) -> Duration {
        self.client
            .timeout()
            .checked_add(REGISTRY_TIMEOUT_GRACE)
            .unwrap_or(self.client.timeout())
    }

    async fn invoke(&self, raw: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let arguments = match raw {
            Value::Object(map) => Some(strip_nulls_object(map)),
            _ => None,
        };
        // `ctx.vision` decides whether an image can leave the call as a content
        // block (US-011). Fail-closed: a model that does not declare vision never
        // gets one sent on its behalf.
        match self
            .client
            .call(&self.original_name, arguments, ctx.vision)
            .await
        {
            // Functional failure: the model sees it and can react.
            Ok(outcome) => {
                let mut output = if outcome.is_error {
                    ToolOutput::error(outcome.text)
                } else {
                    ToolOutput::text(outcome.text)
                };
                if let Some(structured) = outcome.structured_content {
                    output = output.with_structured_content(structured);
                }
                if !outcome.images.is_empty() {
                    output = output.with_images(outcome.images);
                }
                Ok(output)
            }
            // Transport/protocol failure: a pipeline error, whose message names
            // the server (`McpError::Call`).
            Err(err) => Err(ToolError::Io(err.to_string())),
        }
    }
}

/// Stable rendering of a call's arguments, used as the tail of the approval key.
///
/// Two properties are load-bearing. Nulls are stripped exactly as `invoke`
/// strips them, so what is remembered is what was actually sent. And the
/// rendering is order-independent: `serde_json::Map` is a `BTreeMap` (the
/// `preserve_order` feature is off), so the same object always serializes the
/// same way whatever order the model emitted its fields in. The test below is
/// what keeps that second property from silently disappearing behind a feature
/// flag enabled elsewhere in the dependency graph.
fn canonical_arguments(raw: &Value) -> String {
    strip_nulls(raw.clone()).to_string()
}

/// Approval level a tool actually starts from: the configured level, hardened by
/// the server's own `destructiveHint`.
///
/// This is the ONE place an MCP annotation is read, and it is read in a single
/// direction. `destructiveHint: true` forces the confirmation back on even when
/// the configuration auto-approved the tool: a server is allowed to say "this
/// one is dangerous". The reverse (`readOnlyHint: true` granting an
/// auto-approval) is deliberately not implemented, because that is the exact
/// shape of CVE-2025-6514: a compromised server would relabel its own tools to
/// escape the prompt.
fn effective_approval(configured: McpApproval, destructive_hint: Option<bool>) -> McpApproval {
    if destructive_hint == Some(true) {
        McpApproval::Ask
    } else {
        configured
    }
}

/// Baseline decision of an MCP tool for its configured approval level. Split out
/// of the trait method so the decision stays testable without a live connection.
fn baseline_permission(approval: McpApproval) -> PermissionDecision {
    match approval {
        McpApproval::Allow => PermissionDecision::Allow,
        McpApproval::Ask => PermissionDecision::Ask,
    }
}

/// Why a tool listed by a server was not registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolSkipped {
    pub server: String,
    pub tool: String,
    pub reason: String,
}

impl McpToolSkipped {
    pub fn summary(&self) -> String {
        format!(
            "MCP \"{}\": tool \"{}\" not exposed ({})",
            self.server, self.tool, self.reason
        )
    }
}

/// A tool as it will be exposed: the whole naming and schema decision, taken
/// without any connection so it stays directly testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolPlan {
    /// Name exposed to the model.
    pub name: String,
    /// Name to call on the server.
    pub original_name: String,
    pub description: String,
    pub input_schema: Value,
    /// `annotations.destructiveHint`, carried through so the approval level can
    /// be resolved from the plan alone. Read in one direction only, see
    /// `effective_approval`.
    pub destructive_hint: Option<bool>,
}

/// Applies the server policy to the listed tools (US-014): the allow-list runs
/// first, the deny-list second, order taken from Codex. Returns what stays
/// exposed and the diagnostics for the human.
///
/// A filtered-out tool produces NO diagnostic: hiding it is the point. What is
/// reported is a name the configuration mentions and the server does not expose
/// (AC3, a typo silently shrinking the surface), and a filter that empties a
/// server entirely (AC5, which otherwise looks like a broken connection).
pub fn filter_tools(
    server: &str,
    tools: &[McpToolInfo],
    policy: &McpToolPolicy,
) -> (Vec<McpToolInfo>, Vec<String>) {
    if policy.is_default() {
        return (tools.to_vec(), Vec::new());
    }
    let available: BTreeSet<&str> = tools
        .iter()
        .map(|tool| tool.original_name.as_str())
        .collect();
    let mut notices: Vec<String> = policy
        .unknown_names(&available)
        .into_iter()
        .map(|name| {
            format!("MCP \"{server}\": listed tool \"{name}\" is not exposed by the server")
        })
        .collect();
    let kept: Vec<McpToolInfo> = tools
        .iter()
        .filter(|tool| policy.exposes(&tool.original_name))
        .cloned()
        .collect();
    if kept.is_empty() && !tools.is_empty() {
        notices.push(format!(
            "MCP \"{server}\": 0 tool exposed after filtering (server still connected)"
        ));
    }
    (kept, notices)
}

/// Wraps the tools of one connected server as `DynTool`. `tools` is expected to
/// be the already filtered list (`filter_tools`); `policy` carries the approval
/// levels and the parallelism declaration, and nothing about the transport,
/// which this layer has no business knowing.
pub fn dyn_tools(
    server: &str,
    tools: &[McpToolInfo],
    policy: &McpServerPolicy,
    client: &McpClient,
    taken: &mut BTreeSet<String>,
) -> (Vec<Box<dyn DynTool>>, Vec<McpToolSkipped>) {
    let (plans, skipped) = plan_tools(server, tools, taken);
    let registered = plans
        .into_iter()
        .map(|plan| {
            let approval = effective_approval(
                policy.tools.approval_for(&plan.original_name),
                plan.destructive_hint,
            );
            Box::new(McpTool {
                name: plan.name,
                server: server.to_string(),
                original_name: plan.original_name,
                description: plan.description,
                input_schema: plan.input_schema,
                approval,
                supports_parallel: policy.supports_parallel_tool_calls,
                client: client.clone(),
            }) as Box<dyn DynTool>
        })
        .collect();
    (registered, skipped)
}

/// Decides what each tool of one server is exposed as. `taken` accumulates the
/// names already handed out, across every server: uniqueness is a property of the
/// whole set, not of one server.
pub fn plan_tools(
    server: &str,
    tools: &[McpToolInfo],
    taken: &mut BTreeSet<String>,
) -> (Vec<McpToolPlan>, Vec<McpToolSkipped>) {
    let mut plans = Vec::new();
    let mut skipped = Vec::new();
    for info in tools {
        let skip = |reason: &str| McpToolSkipped {
            server: server.to_string(),
            tool: info.original_name.clone(),
            reason: reason.to_string(),
        };
        let Some(schema) = strict_input_schema(&info.input_schema) else {
            skipped.push(skip("input schema cannot be exposed in strict mode"));
            continue;
        };
        let schema_bytes = estimate_json_bytes(&schema);
        if schema_bytes > MAX_SCHEMA_BYTES {
            skipped.push(skip(&format!(
                "input schema too large ({schema_bytes} bytes > {MAX_SCHEMA_BYTES})"
            )));
            continue;
        }
        let name = qualified_name(server, &info.original_name, taken);
        // `initialize.instructions` is deliberately NOT folded in here. A
        // description reaches the model inside the tool definitions, which no
        // tool output ever taints, so server prose smuggled into one would be
        // injection with the taint defense structurally unable to see it. Like
        // resources, it is offered to inspection instead (`/mcp <server> info`).
        let description = if info.description.trim().is_empty() {
            format!(
                "Tool \"{}\" exposed by the MCP server \"{server}\".",
                info.original_name
            )
        } else {
            info.description.clone()
        };
        // Last safety net: the spec is the exact object the provider will send.
        // A tool that still fails validation is dropped here rather than making
        // the whole turn fail (`CanonicalRequest::validate`).
        let spec = ToolSpec::function(name.clone(), description.clone(), schema.clone());
        if let Err(err) = spec.validate() {
            skipped.push(skip(&err.to_string()));
            continue;
        }
        taken.insert(name.clone());
        plans.push(McpToolPlan {
            name,
            original_name: info.original_name.clone(),
            description,
            input_schema: schema,
            destructive_hint: info.destructive_hint,
        });
    }
    (plans, skipped)
}

/// Drops the `null` entries the strict rewrite made possible, so an omitted
/// optional argument stays omitted for the server.
fn strip_nulls_object(map: Map<String, Value>) -> Map<String, Value> {
    map.into_iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key, strip_nulls(value)))
        .collect()
}

fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(strip_nulls_object(map)),
        Value::Array(items) => Value::Array(items.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, schema: Value) -> McpToolInfo {
        McpToolInfo {
            name: name.to_string(),
            original_name: name.to_string(),
            title: None,
            description: "desc".to_string(),
            input_schema: schema,
            output_schema: None,
            annotations_untrusted: true,
            destructive_hint: None,
        }
    }

    fn object_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "limit": { "type": "integer" }
            },
            "required": ["path"]
        })
    }

    #[test]
    fn nulls_are_stripped_before_the_call() {
        let mut map = Map::new();
        map.insert("path".to_string(), Value::String("x".to_string()));
        map.insert("limit".to_string(), Value::Null);
        map.insert(
            "nested".to_string(),
            serde_json::json!({"a": null, "b": 1, "c": [{"d": null, "e": 2}]}),
        );
        let stripped = strip_nulls_object(map);
        assert!(!stripped.contains_key("limit"));
        assert_eq!(stripped["path"], "x");
        assert_eq!(
            stripped["nested"],
            serde_json::json!({"b": 1, "c": [{"e": 2}]})
        );
    }

    #[test]
    fn an_unusable_schema_skips_only_its_own_tool() {
        let mut taken = BTreeSet::new();
        let (plans, skipped) = plan_tools(
            "srv",
            &[
                info("ok", object_schema()),
                info("bad", serde_json::json!({"type": "array"})),
            ],
            &mut taken,
        );
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].name, "mcp__srv__ok");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].tool, "bad");
        assert!(skipped[0].summary().contains("not exposed"));
        assert_eq!(taken, BTreeSet::from(["mcp__srv__ok".to_string()]));
    }

    #[test]
    fn an_oversized_schema_is_refused() {
        let mut properties = Map::new();
        for i in 0..2_000 {
            properties.insert(
                format!("field_{i}"),
                serde_json::json!({"type": "string", "description": "padding padding padding"}),
            );
        }
        let schema = serde_json::json!({"type": "object", "properties": properties});
        let mut taken = BTreeSet::new();
        let (plans, skipped) = plan_tools("srv", &[info("huge", schema)], &mut taken);
        assert!(plans.is_empty());
        assert!(skipped[0].reason.contains("input schema too large"));
    }

    #[test]
    fn an_empty_description_gets_a_usable_fallback() {
        let mut listed = info("read", object_schema());
        listed.description = "  ".to_string();
        let mut taken = BTreeSet::new();
        let (plans, _) = plan_tools("files", &[listed], &mut taken);
        assert!(plans[0].description.contains("\"read\""));
        assert!(plans[0].description.contains("\"files\""));
    }

    #[test]
    fn the_allow_list_runs_before_the_deny_list_on_the_listed_set() {
        let listed = [
            info("read", object_schema()),
            info("write", object_schema()),
            info("delete", object_schema()),
        ];
        let policy = McpToolPolicy {
            enabled: Some(BTreeSet::from(["read".into(), "write".into()])),
            disabled: BTreeSet::from(["write".into()]),
            ..McpToolPolicy::default()
        };
        let (kept, notices) = filter_tools("srv", &listed, &policy);
        assert_eq!(
            kept.iter()
                .map(|tool| tool.original_name.as_str())
                .collect::<Vec<_>>(),
            vec!["read"]
        );
        assert!(notices.is_empty(), "{notices:?}");
    }

    #[test]
    fn an_unfiltered_server_keeps_every_tool_and_says_nothing() {
        let listed = [info("read", object_schema())];
        let (kept, notices) = filter_tools("srv", &listed, &McpToolPolicy::default());
        assert_eq!(kept.len(), 1);
        assert!(notices.is_empty());
    }

    #[test]
    fn a_listed_name_absent_from_the_server_is_reported_without_failing() {
        let listed = [info("read", object_schema())];
        let policy = McpToolPolicy {
            disabled: BTreeSet::from(["ghost".into()]),
            ..McpToolPolicy::default()
        };
        let (kept, notices) = filter_tools("srv", &listed, &policy);
        assert_eq!(kept.len(), 1, "the connection is not affected");
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("ghost"), "{}", notices[0]);
    }

    #[test]
    fn a_filter_that_empties_a_server_is_reported_not_hidden() {
        let listed = [info("read", object_schema())];
        let policy = McpToolPolicy {
            disabled: BTreeSet::from(["read".into()]),
            ..McpToolPolicy::default()
        };
        let (kept, notices) = filter_tools("srv", &listed, &policy);
        assert!(kept.is_empty());
        assert!(
            notices.iter().any(|n| n.contains("0 tool exposed")),
            "{notices:?}"
        );
    }

    #[test]
    fn an_auto_approved_tool_still_confirms_under_taint() {
        use agent_tools::permission::{PermissionMode, Resolved, resolve_permission};

        let allow = baseline_permission(McpApproval::Allow);
        assert_eq!(allow, PermissionDecision::Allow);
        assert_eq!(
            baseline_permission(McpApproval::Ask),
            PermissionDecision::Ask
        );
        // Without taint the configured level is honored.
        assert_eq!(
            resolve_permission(PermissionMode::Default, allow, false, true, true, false),
            Resolved::Allow
        );
        // US-015 AC3: recent untrusted taint forces the confirmation back. No MCP
        // setting can weaken the OWASP LLM01 defense.
        assert_eq!(
            resolve_permission(PermissionMode::Default, allow, false, true, true, true),
            Resolved::Ask
        );
    }

    #[test]
    fn a_destructive_hint_hardens_an_auto_approved_tool() {
        // The configuration auto-approved it; the server says it is destructive.
        // The confirmation comes back: an annotation may aggravate.
        assert_eq!(
            effective_approval(McpApproval::Allow, Some(true)),
            McpApproval::Ask
        );
        // The reverse is refused by construction: nothing a server declares can
        // grant an auto-approval (CVE-2025-6514).
        assert_eq!(
            effective_approval(McpApproval::Ask, Some(false)),
            McpApproval::Ask
        );
        assert_eq!(effective_approval(McpApproval::Ask, None), McpApproval::Ask);
        // No annotation, configured allow: the human decision stands.
        assert_eq!(
            effective_approval(McpApproval::Allow, None),
            McpApproval::Allow
        );
    }

    /// A description is what the tool itself said, and nothing else. Server prose
    /// (`initialize.instructions`) reaches the model nowhere, because a tool
    /// definition is not a tool output and therefore never taints the turn: text
    /// smuggled in here would be injection the taint defense cannot see.
    #[test]
    fn a_description_carries_only_what_the_tool_declared() {
        let mut taken = BTreeSet::new();
        let (plans, _) = plan_tools("docs", &[info("search", object_schema())], &mut taken);
        assert_eq!(plans[0].description, "desc");
    }

    /// The arguments are part of the approval key, so one answer authorizes one
    /// act rather than a tool name (CVE-2026-22708 transposed to MCP).
    #[test]
    fn the_approval_key_separates_two_calls_of_the_same_tool() {
        let benign = canonical_arguments(&serde_json::json!({"path": "notes.md"}));
        let hostile = canonical_arguments(&serde_json::json!({"path": "~/.ssh/authorized_keys"}));
        assert_ne!(benign, hostile);
        // Field order is not part of the act: the model emitting the same call
        // twice in a different order must not be asked twice.
        assert_eq!(
            canonical_arguments(&serde_json::json!({"a": 1, "b": 2})),
            canonical_arguments(&serde_json::json!({"b": 2, "a": 1}))
        );
        // An omitted field and an explicit null are the same act, because
        // `invoke` strips nulls before sending.
        assert_eq!(
            canonical_arguments(&serde_json::json!({"a": 1, "b": null})),
            canonical_arguments(&serde_json::json!({"a": 1}))
        );
    }

    #[test]
    fn parallelism_and_approval_come_from_the_server_entry() {
        let listed = [
            info("read", object_schema()),
            McpToolInfo {
                destructive_hint: Some(true),
                ..info("wipe", object_schema())
            },
        ];
        let policy = McpServerPolicy {
            supports_parallel_tool_calls: true,
            tools: McpToolPolicy {
                default_approval: McpApproval::Allow,
                ..McpToolPolicy::default()
            },
            ..McpServerPolicy::default()
        };
        let plans = plan_tools("srv", &listed, &mut BTreeSet::new()).0;
        assert_eq!(plans.len(), 2);
        // The plan carries the hint, so the approval is resolved from the plan
        // alone: no second lookup against the listing, hence no way for the two
        // to drift apart.
        let approvals: Vec<McpApproval> = plans
            .iter()
            .map(|plan| {
                effective_approval(
                    policy.tools.approval_for(&plan.original_name),
                    plan.destructive_hint,
                )
            })
            .collect();
        assert_eq!(approvals, vec![McpApproval::Allow, McpApproval::Ask]);
        assert!(policy.supports_parallel_tool_calls);
    }

    #[test]
    fn two_servers_exposing_the_same_tool_keep_distinct_registrations() {
        let mut taken = BTreeSet::new();
        let (alpha, _) = plan_tools("alpha", &[info("search", object_schema())], &mut taken);
        let (beta, _) = plan_tools("beta", &[info("search", object_schema())], &mut taken);
        assert_eq!(alpha[0].original_name, "search");
        assert_eq!(beta[0].original_name, "search");
        assert_ne!(alpha[0].name, beta[0].name);
    }
}
