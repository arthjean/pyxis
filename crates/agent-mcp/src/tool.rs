//! MCP tools exposed to the model as `DynTool` (US-011). Three problems are
//! solved here, in this order, because each one silently breaks the turn when it
//! is not:
//!
//! 1. **Naming.** The model API accepts `^[A-Za-z0-9_-]+$` over at most 64 bytes.
//!    `mcp__{server}__{tool}` overflows as soon as both names are long, so the
//!    composed name is sanitized, then shortened deterministically with a
//!    fingerprint of the pair, and uniqueness is verified at registration. The
//!    mapping back to the origin server and tool is kept on the tool itself, so a
//!    shortened name still reaches the right server.
//! 2. **Schema.** The provider emits `strict: true` and `ToolSpec::validate`
//!    enforces it (`additionalProperties: false`, every property in `required`).
//!    An MCP schema almost never complies, and an invalid spec kills the whole
//!    turn. Schemas are therefore rewritten into the strict form, and a tool whose
//!    schema resists is dropped with a diagnostic rather than left to break the
//!    session.
//! 3. **Trust.** Everything coming from a server is untrusted (CVE-2025-6514):
//!    fail-closed metadata, `Ask` baseline, taint propagated in full.

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
use crate::config::{McpApproval, McpToolPolicy};

/// Prefix of every registered MCP tool. Its presence makes a collision with a
/// native tool impossible.
pub const NAME_PREFIX: &str = "mcp__";
/// Name cap imposed by the model API (`ToolSpec::validate`).
pub const MAX_NAME_BYTES: usize = 64;
/// Bound of an exposed input schema: a server must not be able to flood the
/// prompt through its schemas any more than through its descriptions
/// (ARCHITECTURE 6).
pub const MAX_SCHEMA_BYTES: usize = 16_384;
/// Depth bound of the schema rewrite: a hostile server does not get to blow the
/// stack with nesting.
const MAX_SCHEMA_DEPTH: u32 = 24;
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
    /// Approval level resolved from the server policy (US-015). It is a
    /// *baseline*: the permission mode, the hooks and the taint defense sit above
    /// it and can only tighten it.
    approval: McpApproval,
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

    // ───── Fail-closed metadata (invariant 4). None of it is derived from the
    // server's `annotations`: those are hints from a remote party, never a
    // security decision on our side.
    fn is_concurrency_safe(&self) -> bool {
        false
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

    /// No answer is ever remembered for an MCP call: the arguments are free-form,
    /// so no token sequence identifies "the same act" the way an argv does.
    fn approval_memo(&self, _raw: &Value) -> ApprovalMemo {
        ApprovalMemo::NotApplicable
    }

    fn timeout(&self, _ctx: &ToolCtx) -> Duration {
        self.client
            .timeout()
            .checked_add(REGISTRY_TIMEOUT_GRACE)
            .unwrap_or(self.client.timeout())
    }

    async fn invoke(&self, raw: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let arguments = match raw {
            Value::Object(map) => Some(strip_nulls_object(map)),
            _ => None,
        };
        match self.client.call(&self.original_name, arguments).await {
            // Functional failure: the model sees it and can react.
            Ok(outcome) if outcome.is_error => Ok(ToolOutput::error(outcome.text)),
            Ok(outcome) => Ok(ToolOutput::text(outcome.text)),
            // Transport/protocol failure: a pipeline error, whose message names
            // the server (`McpError::Call`).
            Err(err) => Err(ToolError::Io(err.to_string())),
        }
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

/// Wraps the tools of one connected server as `DynTool`. `tools` is expected to be
/// the already filtered list (`filter_tools`); `policy` is read here for the
/// approval level alone.
pub fn dyn_tools(
    server: &str,
    tools: &[McpToolInfo],
    policy: &McpToolPolicy,
    client: &McpClient,
    taken: &mut BTreeSet<String>,
) -> (Vec<Box<dyn DynTool>>, Vec<McpToolSkipped>) {
    let (plans, skipped) = plan_tools(server, tools, taken);
    let registered = plans
        .into_iter()
        .map(|plan| {
            let approval = policy.approval_for(&plan.original_name);
            Box::new(McpTool {
                name: plan.name,
                server: server.to_string(),
                original_name: plan.original_name,
                description: plan.description,
                input_schema: plan.input_schema,
                approval,
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
        let spec = ToolSpec {
            name: name.clone(),
            description: description.clone(),
            input_schema: schema.clone(),
        };
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
        });
    }
    (plans, skipped)
}

/// Composes the exposed name: `mcp__{server}__{tool}`, sanitized, shortened
/// deterministically when it overflows, and unique within `taken`.
pub fn qualified_name(server: &str, tool: &str, taken: &BTreeSet<String>) -> String {
    let base = format!(
        "{NAME_PREFIX}{}__{}",
        sanitize_part(server),
        sanitize_part(tool)
    );
    let fingerprint = fingerprint(server, tool);
    // Bounded by construction: `taken` is finite, so a free name is reached in at
    // most `taken.len() + 1` attempts.
    let mut attempt = 0_usize;
    loop {
        let candidate = if attempt == 0 && base.len() <= MAX_NAME_BYTES {
            base.clone()
        } else if attempt == 0 {
            shorten(&base, &format!("_{fingerprint}"))
        } else {
            shorten(&base, &format!("_{fingerprint}_{attempt}"))
        };
        if !taken.contains(&candidate) {
            return candidate;
        }
        attempt += 1;
    }
}

/// Keeps only what the model API accepts; anything else becomes `_`. The result
/// is pure ASCII, hence safe to cut on a byte boundary.
fn sanitize_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "_".to_string()
    } else {
        sanitized
    }
}

/// Cuts `base` so that `base_prefix + suffix` fits in `MAX_NAME_BYTES`. `base` is
/// ASCII (post-sanitize), so a byte cut is a character cut.
fn shorten(base: &str, suffix: &str) -> String {
    let keep = MAX_NAME_BYTES.saturating_sub(suffix.len());
    let mut out: String = base.chars().take(keep).collect();
    out.push_str(suffix);
    out
}

/// FNV-1a over `server\0tool`. Hand-rolled on purpose: `DefaultHasher` is not
/// stable across Rust versions, and "deterministic" is the property the shortened
/// name depends on.
fn fingerprint(server: &str, tool: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in server
        .as_bytes()
        .iter()
        .chain(std::iter::once(&0_u8))
        .chain(tool.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", (hash >> 32) as u32)
}

/// Rewrites an MCP input schema into the strict form the model API requires.
/// Returns `None` when the schema cannot be exposed at all (non-object root,
/// nesting past the depth bound).
///
/// A property that was optional is widened to accept `null`, the documented way
/// to keep it optional while `required` lists everything; `invoke` strips those
/// nulls again before the call, so the server never sees an argument the model
/// meant to omit.
pub fn strict_input_schema(schema: &Value) -> Option<Value> {
    let obj = schema.as_object()?;
    let mut root = obj.clone();
    match root.get("type") {
        // MCP mandates an object schema; a missing type is tolerated and pinned.
        None => {
            root.insert("type".to_string(), Value::String("object".to_string()));
        }
        Some(_) if type_includes_object(&root) => {}
        Some(_) => return None,
    }
    normalize(&Value::Object(root), 0)
}

fn normalize(node: &Value, depth: u32) -> Option<Value> {
    if depth > MAX_SCHEMA_DEPTH {
        return None;
    }
    let Some(obj) = node.as_object() else {
        return Some(node.clone());
    };
    let mut out = obj.clone();
    let originally_required: BTreeSet<String> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Some(Value::Object(props)) = obj.get("properties") {
        let mut rewritten = Map::new();
        for (key, value) in props {
            let mut child = normalize(value, depth + 1)?;
            if !originally_required.contains(key) {
                child = widen_nullable(child);
            }
            rewritten.insert(key.clone(), child);
        }
        out.insert("properties".to_string(), Value::Object(rewritten));
    }
    for key in ["items", "additionalItems", "contains"] {
        if let Some(value) = obj.get(key) {
            out.insert(key.to_string(), normalize(value, depth + 1)?);
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(items)) = obj.get(key) {
            let mut rewritten = Vec::with_capacity(items.len());
            for item in items {
                rewritten.push(normalize(item, depth + 1)?);
            }
            out.insert(key.to_string(), Value::Array(rewritten));
        }
    }
    for key in ["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = obj.get(key) {
            let mut rewritten = Map::new();
            for (name, value) in defs {
                rewritten.insert(name.clone(), normalize(value, depth + 1)?);
            }
            out.insert(key.to_string(), Value::Object(rewritten));
        }
    }
    if type_includes_object(&out) {
        let names: Vec<Value> = out
            .get("properties")
            .and_then(Value::as_object)
            .map(|props| props.keys().cloned().map(Value::String).collect())
            .unwrap_or_default();
        if !out.contains_key("properties") {
            out.insert("properties".to_string(), Value::Object(Map::new()));
        }
        out.insert("additionalProperties".to_string(), Value::Bool(false));
        out.insert("required".to_string(), Value::Array(names));
    }
    Some(Value::Object(out))
}

/// Adds `null` to the declared type of an optional property. A property without a
/// declared type (a `$ref`, a bare `enum`) is left untouched: widening it would
/// mean guessing its shape.
fn widen_nullable(node: Value) -> Value {
    let Value::Object(mut obj) = node else {
        return node;
    };
    let widened = match obj.get("type") {
        Some(Value::String(single)) if single != "null" => Some(Value::Array(vec![
            Value::String(single.clone()),
            Value::String("null".to_string()),
        ])),
        Some(Value::Array(kinds)) if !kinds.iter().any(|kind| kind.as_str() == Some("null")) => {
            let mut kinds = kinds.clone();
            kinds.push(Value::String("null".to_string()));
            Some(Value::Array(kinds))
        }
        _ => None,
    };
    if let Some(widened) = widened {
        obj.insert("type".to_string(), widened);
    }
    Value::Object(obj)
}

fn type_includes_object(schema: &Map<String, Value>) -> bool {
    match schema.get("type") {
        Some(Value::String(kind)) => kind == "object",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some("object")),
        _ => false,
    }
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
    fn name_is_prefixed_by_its_server() {
        let taken = BTreeSet::new();
        assert_eq!(qualified_name("files", "read", &taken), "mcp__files__read");
    }

    #[test]
    fn same_tool_on_two_servers_gets_two_names() {
        let mut taken = BTreeSet::new();
        let a = qualified_name("alpha", "search", &taken);
        taken.insert(a.clone());
        let b = qualified_name("beta", "search", &taken);
        assert_ne!(a, b);
        assert_eq!(a, "mcp__alpha__search");
        assert_eq!(b, "mcp__beta__search");
    }

    #[test]
    fn long_names_are_shortened_deterministically_and_stay_unique() {
        let server = "a".repeat(60);
        let taken = BTreeSet::new();
        let first = qualified_name(&server, &"b".repeat(60), &taken);
        let second = qualified_name(&server, &"b".repeat(61), &taken);
        assert!(first.len() <= MAX_NAME_BYTES, "{} bytes", first.len());
        assert!(second.len() <= MAX_NAME_BYTES);
        // Same shared prefix, different fingerprint -> no silent collision.
        assert_ne!(first, second);
        assert_eq!(first, qualified_name(&server, &"b".repeat(60), &taken));
    }

    #[test]
    fn a_taken_name_forces_a_distinct_candidate() {
        let mut taken = BTreeSet::new();
        taken.insert("mcp__files__read".to_string());
        let name = qualified_name("files", "read", &taken);
        assert_ne!(name, "mcp__files__read");
        assert!(name.starts_with("mcp__files__read"));
        assert!(name.len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn out_of_charset_characters_are_sanitized() {
        let taken = BTreeSet::new();
        let name = qualified_name("my server", "tool.v2:beta", &taken);
        assert_eq!(name, "mcp__my_server__tool_v2_beta");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
    }

    #[test]
    fn sanitizing_does_not_create_a_silent_collision() {
        let mut taken = BTreeSet::new();
        let a = qualified_name("srv", "tool.v2", &taken);
        taken.insert(a.clone());
        let b = qualified_name("srv", "tool:v2", &taken);
        assert_ne!(a, b, "two distinct tools must keep two names");
    }

    #[test]
    fn strict_schema_requires_every_property_and_denies_extras() {
        let schema = strict_input_schema(&object_schema()).unwrap();
        assert_eq!(schema["additionalProperties"], Value::Bool(false));
        let required: BTreeSet<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required, BTreeSet::from(["path", "limit"]));
        // Originally optional -> nullable, so the model can still omit it.
        assert_eq!(
            schema["properties"]["limit"]["type"],
            serde_json::json!(["integer", "null"])
        );
        assert_eq!(schema["properties"]["path"]["type"], "string");
    }

    #[test]
    fn strict_schema_recurses_into_nested_objects() {
        let schema = strict_input_schema(&serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "object",
                    "properties": { "kind": { "type": "string" } }
                }
            },
            "required": ["filter"]
        }))
        .unwrap();
        let nested = &schema["properties"]["filter"];
        assert_eq!(nested["additionalProperties"], Value::Bool(false));
        assert_eq!(nested["required"], serde_json::json!(["kind"]));
    }

    #[test]
    fn missing_type_is_pinned_and_non_object_root_is_refused() {
        let pinned = strict_input_schema(&serde_json::json!({"properties": {}})).unwrap();
        assert_eq!(pinned["type"], "object");
        assert!(strict_input_schema(&serde_json::json!({"type": "string"})).is_none());
        assert!(strict_input_schema(&serde_json::json!("nope")).is_none());
    }

    #[test]
    fn deep_nesting_is_refused_instead_of_blowing_the_stack() {
        let mut schema = serde_json::json!({"type": "object", "properties": {}});
        for _ in 0..(MAX_SCHEMA_DEPTH + 4) {
            schema = serde_json::json!({
                "type": "object",
                "properties": { "next": schema },
                "required": ["next"]
            });
        }
        assert!(strict_input_schema(&schema).is_none());
    }

    #[test]
    fn normalized_schemas_pass_the_provider_validation() {
        let schema = strict_input_schema(&object_schema()).unwrap();
        let spec = ToolSpec {
            name: "mcp__files__read".to_string(),
            description: "d".to_string(),
            input_schema: schema,
        };
        spec.validate().unwrap();
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
    fn two_servers_exposing_the_same_tool_keep_distinct_registrations() {
        let mut taken = BTreeSet::new();
        let (alpha, _) = plan_tools("alpha", &[info("search", object_schema())], &mut taken);
        let (beta, _) = plan_tools("beta", &[info("search", object_schema())], &mut taken);
        assert_eq!(alpha[0].original_name, "search");
        assert_eq!(beta[0].original_name, "search");
        assert_ne!(alpha[0].name, beta[0].name);
    }
}
