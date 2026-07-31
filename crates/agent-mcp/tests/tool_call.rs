//! End-to-end proof of EP-003: a real stdio server is spawned, its tools are
//! registered in the `agent-tools` Registry, and the model-side dispatch reaches
//! them. Covers the paths a unit test cannot: functional error vs transport
//! error, timeout, server death in flight, routing between two servers, taint.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use agent_core::tools::ToolInvocation;
use agent_mcp::{McpConnection, McpServerConfig, McpServerPolicy, McpToolInfo};
use agent_tools::Registry;
use agent_tools::permission::{
    ApprovalResponse, Approver, PermissionMode, PermissionRequest, Resolved, resolve_permission,
};
use async_trait::async_trait;

fn temp_dir(tag: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let dir = std::env::temp_dir().join(format!("pyxis-mcp-{}-{millis}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture_exe_name() -> &'static str {
    if cfg!(windows) {
        "mcp_call_fixture.exe"
    } else {
        "mcp_call_fixture"
    }
}

fn compile_fixture(dir: &Path) -> PathBuf {
    let src = dir.join("mcp_call_fixture.rs");
    let exe = dir.join(fixture_exe_name());
    std::fs::write(&src, FIXTURE_SRC).unwrap();
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("--edition=2021")
        .arg("-Awarnings")
        .arg(&src)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("rustc fixture doit se lancer");
    assert!(
        output.status.success(),
        "fixture rustc failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    exe
}

fn config(exe: &Path) -> McpServerConfig {
    McpServerConfig::stdio(exe.to_string_lossy().into_owned(), Vec::new())
}

async fn connect(exe: &Path, name: &str) -> (McpConnection, Vec<McpToolInfo>) {
    let conn = McpConnection::connect(name, &config(exe)).await.unwrap();
    let tools = conn.list_tools(name).await.unwrap();
    (conn, tools)
}

/// Approver that records what it was asked and always accepts once.
#[derive(Default)]
struct RecordingApprover {
    asked: AtomicUsize,
    tainted: AtomicUsize,
    memoizable: AtomicUsize,
}

#[async_trait]
impl Approver for RecordingApprover {
    async fn approve(&self, req: &PermissionRequest) -> ApprovalResponse {
        self.asked.fetch_add(1, Ordering::SeqCst);
        if req.taint_forced {
            self.tainted.fetch_add(1, Ordering::SeqCst);
        }
        if req.memoizable {
            self.memoizable.fetch_add(1, Ordering::SeqCst);
        }
        ApprovalResponse::ALLOW_ONCE
    }
}

fn call(id: &str, name: &str, input: serde_json::Value) -> ToolInvocation {
    ToolInvocation::json(id, name, input)
}

#[tokio::test]
async fn mcp_tools_are_registered_and_callable_by_the_model() {
    let dir = temp_dir("call-registry");
    let exe = compile_fixture(&dir);
    let (conn, listed) = connect(&exe, "fixture").await;
    assert_eq!(listed.len(), 4, "the fixture exposes 4 tools");

    let client = conn.client("fixture");
    let mut taken = BTreeSet::new();
    let (tools, skipped) = agent_mcp::dyn_tools(
        "fixture",
        &listed,
        &McpServerPolicy::default(),
        &client,
        &mut taken,
    );
    assert!(skipped.is_empty(), "unexpected skips: {skipped:?}");
    assert_eq!(tools.len(), 4);

    let approver = Arc::new(RecordingApprover::default());
    let mut builder = Registry::builder(&dir)
        .mode(PermissionMode::Default)
        .approver(approver.clone());
    for tool in tools {
        builder = builder.register_dyn(tool);
    }
    let registry = builder.build();

    let names: Vec<String> = registry
        .tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert!(
        names.contains(&"mcp__fixture__echo".to_string()),
        "{names:?}"
    );

    // Nominal call: the text of the server reaches the model.
    let outcomes = registry
        .dispatch(vec![call(
            "1",
            "mcp__fixture__echo",
            serde_json::json!({"text": "salut", "loud": null}),
        )])
        .await;
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].content, "echo: salut");
    assert!(!outcomes[0].is_error);
    // US-013: every MCP result is untrusted, and a confirmation was requested.
    assert!(outcomes[0].untrusted, "an MCP result is always untrusted");
    assert_eq!(approver.asked.load(Ordering::SeqCst), 1);

    // Functional failure: a tool result in error, NOT a protocol error.
    let outcomes = registry
        .dispatch(vec![call("2", "mcp__fixture__boom", serde_json::json!({}))])
        .await;
    assert!(outcomes[0].is_error);
    assert_eq!(outcomes[0].content, "tool failed");
    assert_eq!(
        outcomes[0].error_kind,
        Some(agent_core::message::ToolErrorKind::Semantic),
        "a functional failure is semantic, not a pipeline error"
    );
    assert!(outcomes[0].untrusted);

    // Non-textual content: a bounded descriptor, never the raw payload.
    let outcomes = registry
        .dispatch(vec![call(
            "3",
            "mcp__fixture__picture",
            serde_json::json!({}),
        )])
        .await;
    assert!(
        outcomes[0].content.starts_with("[mcp image omitted:"),
        "{}",
        outcomes[0].content
    );
    assert!(!outcomes[0].content.contains("AAAA"));

    conn.cancel().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_stalled_server_expires_and_the_error_names_it() {
    let dir = temp_dir("call-timeout");
    let exe = compile_fixture(&dir);
    let (conn, _listed) = connect(&exe, "sleepy").await;
    let client = conn
        .client("sleepy")
        .with_timeout(Duration::from_millis(300));

    let started = std::time::Instant::now();
    let err = client.call("stall", None, false).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5), "bounded wait");
    let message = err.to_string();
    assert!(message.contains("sleepy"), "{message}");
    assert!(message.contains("timeout"), "{message}");

    // The connection stays usable after an expiry.
    let ok = client.call("echo", None, false).await.unwrap();
    assert!(ok.text.starts_with("echo: "), "{}", ok.text);

    conn.cancel().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_server_dying_in_flight_errors_without_panicking() {
    let dir = temp_dir("call-death");
    let exe = compile_fixture(&dir);
    let (conn, _listed) = connect(&exe, "doomed").await;
    let client = conn.client("doomed");

    let err = client.call("die", None, false).await.unwrap_err();
    let message = err.to_string();
    assert!(message.contains("doomed"), "{message}");

    // Any later call fails the same way rather than hanging.
    let again = client.call("echo", None, false).await.unwrap_err();
    assert!(again.to_string().contains("doomed"));

    conn.cancel().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn two_servers_exposing_the_same_tool_each_get_their_own_call() {
    let dir = temp_dir("call-routing");
    let exe = compile_fixture(&dir);
    let (alpha_conn, alpha_listed) = connect(&exe, "alpha").await;
    let (beta_conn, beta_listed) = connect(&exe, "beta").await;

    let mut taken = BTreeSet::new();
    let alpha_client = alpha_conn.client("alpha");
    let beta_client = beta_conn.client("beta");
    let (alpha_tools, _) = agent_mcp::dyn_tools(
        "alpha",
        &alpha_listed,
        &McpServerPolicy::default(),
        &alpha_client,
        &mut taken,
    );
    let (beta_tools, _) = agent_mcp::dyn_tools(
        "beta",
        &beta_listed,
        &McpServerPolicy::default(),
        &beta_client,
        &mut taken,
    );

    let mut builder = Registry::builder(&dir)
        .mode(PermissionMode::Default)
        .approver(Arc::new(RecordingApprover::default()));
    for tool in alpha_tools.into_iter().chain(beta_tools) {
        builder = builder.register_dyn(tool);
    }
    let registry = builder.build();
    let names: BTreeSet<String> = registry
        .tool_specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert!(names.contains("mcp__alpha__echo"));
    assert!(names.contains("mcp__beta__echo"));

    // `pid` answers with the pid of the process that served the call: proof the
    // call reached the right subprocess and not the other one.
    let alpha_pid = registry
        .dispatch(vec![call("1", "mcp__alpha__pid", serde_json::json!({}))])
        .await
        .remove(0)
        .content;
    let beta_pid = registry
        .dispatch(vec![call("2", "mcp__beta__pid", serde_json::json!({}))])
        .await
        .remove(0)
        .content;
    assert_ne!(alpha_pid, beta_pid, "two servers, two processes");

    alpha_conn.cancel().await;
    beta_conn.cancel().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Trivial trusted tool, used to run the dispatch cycles that let the taint
/// window expire. It has to succeed and return trusted content: a batch of pure
/// failures deliberately re-marks the taint, so the model cannot wash the window
/// with bogus calls.
struct Trusted;

#[async_trait]
impl agent_tools::tool::Tool for Trusted {
    type Input = serde_json::Value;

    fn name(&self) -> &str {
        "trusted_noop"
    }
    fn description(&self) -> String {
        "test-only no-op".to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false, "required": []})
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    fn returns_untrusted(&self) -> bool {
        false
    }
    fn permission(
        &self,
        _input: &Self::Input,
        _ctx: &agent_tools::permission::PermCtx,
    ) -> agent_tools::permission::PermissionDecision {
        agent_tools::permission::PermissionDecision::Allow
    }
    async fn call(
        &self,
        _input: Self::Input,
        _ctx: &agent_tools::tool::ToolCtx,
    ) -> Result<agent_tools::tool::ToolOutput, agent_tools::error::ToolError> {
        Ok(agent_tools::tool::ToolOutput::text("ok"))
    }
}

/// Runs trusted dispatch cycles until the taint window has expired, so what is
/// under test is the approval memory and not the taint defense that outranks it.
async fn drain_taint(registry: &Registry) {
    for _ in 0..8 {
        if !registry.taint_recent() {
            return;
        }
        let outcomes = registry
            .dispatch(vec![call("drain", "trusted_noop", serde_json::json!({}))])
            .await;
        assert!(!outcomes[0].is_error, "{}", outcomes[0].content);
    }
    assert!(
        !registry.taint_recent(),
        "the taint window must expire within the bound"
    );
}

/// A remembered answer applies to the (server, tool, arguments) triple: the same
/// act is not re-asked, a different one is. Three guardrails, all load-bearing:
/// the answer does not carry over to other arguments, it does not leak to
/// another tool of the same server, and the taint defense outranks it, which is
/// why the window is drained between calls here.
#[tokio::test]
async fn an_answer_is_remembered_per_act_not_per_tool_name() {
    let dir = temp_dir("call-memo");
    let exe = compile_fixture(&dir);
    let (conn, listed) = connect(&exe, "fixture").await;
    let client = conn.client("fixture");
    let mut taken = BTreeSet::new();
    let (tools, _) = agent_mcp::dyn_tools(
        "fixture",
        &listed,
        &McpServerPolicy::default(),
        &client,
        &mut taken,
    );

    // Answers "allow and remember" the first time it is asked.
    #[derive(Default)]
    struct RememberingApprover {
        asked: AtomicUsize,
    }
    #[async_trait]
    impl Approver for RememberingApprover {
        async fn approve(&self, req: &PermissionRequest) -> ApprovalResponse {
            self.asked.fetch_add(1, Ordering::SeqCst);
            assert!(
                req.memoizable,
                "an MCP call is answered per (server, tool, arguments): {req:?}"
            );
            ApprovalResponse::ALLOW_SESSION
        }
    }

    let approver = Arc::new(RememberingApprover::default());
    let mut builder = Registry::builder(&dir)
        .mode(PermissionMode::Default)
        .approver(approver.clone())
        .register(Trusted);
    for tool in tools {
        builder = builder.register_dyn(tool);
    }
    let registry = builder.build();

    // First call: asked once, and the answer is remembered.
    let echo = |id: &str, text: &str| {
        call(
            id,
            "mcp__fixture__echo",
            serde_json::json!({"text": text, "loud": null}),
        )
    };
    let outcomes = registry.dispatch(vec![echo("1", "a")]).await;
    assert_eq!(outcomes[0].content, "echo: a");
    assert_eq!(approver.asked.load(Ordering::SeqCst), 1);

    // The SAME act again: no new question. This is what the memory buys, and the
    // taint window from the first result is cleared first, since the taint
    // defense deliberately outranks the memory (US-008).
    drain_taint(&registry).await;
    let outcomes = registry.dispatch(vec![echo("2", "a")]).await;
    assert_eq!(outcomes[0].content, "echo: a");
    assert_eq!(
        approver.asked.load(Ordering::SeqCst),
        1,
        "a remembered answer must not be asked again for the same act"
    );

    // Same tool, DIFFERENT arguments: a different act, so it is asked. An MCP
    // call has no argv, but its arguments are its entire security surface, and
    // one answer must never authorize a target the human never saw.
    drain_taint(&registry).await;
    let outcomes = registry.dispatch(vec![echo("3", "b")]).await;
    assert_eq!(outcomes[0].content, "echo: b");
    assert_eq!(
        approver.asked.load(Ordering::SeqCst),
        2,
        "the memory of one act must not cover another target"
    );

    // ANOTHER tool of the same server is a different act too.
    drain_taint(&registry).await;
    let outcomes = registry
        .dispatch(vec![call("4", "mcp__fixture__pid", serde_json::json!({}))])
        .await;
    assert!(!outcomes[0].is_error);
    assert_eq!(
        approver.asked.load(Ordering::SeqCst),
        3,
        "the memory of one tool must not cover another"
    );

    conn.cancel().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_mcp_result_taints_the_rest_of_the_turn() {
    let dir = temp_dir("call-taint");
    let exe = compile_fixture(&dir);
    let (conn, listed) = connect(&exe, "fixture").await;
    let client = conn.client("fixture");
    let mut taken = BTreeSet::new();
    let (tools, _) = agent_mcp::dyn_tools(
        "fixture",
        &listed,
        &McpServerPolicy::default(),
        &client,
        &mut taken,
    );

    let approver = Arc::new(RecordingApprover::default());
    let mut builder = Registry::builder(&dir)
        // DontAsk would normally never interrupt: the taint defense must still
        // force a confirmation after an MCP result (invariant 3).
        .mode(PermissionMode::DontAsk)
        .approver(approver.clone());
    for tool in tools {
        builder = builder.register_dyn(tool);
    }
    let registry = builder.build();

    assert!(!registry.taint_recent());
    let outcomes = registry
        .dispatch(vec![call(
            "1",
            "mcp__fixture__echo",
            serde_json::json!({"text": "x"}),
        )])
        .await;
    assert!(outcomes[0].untrusted);
    assert!(
        registry.taint_recent(),
        "an MCP result must mark the taint window"
    );
    // In DontAsk the call itself went through without a question...
    assert_eq!(approver.asked.load(Ordering::SeqCst), 0);
    // ...but the next sensitive action is now forced to ask.
    let outcomes = registry
        .dispatch(vec![call(
            "2",
            "mcp__fixture__echo",
            serde_json::json!({"text": "y"}),
        )])
        .await;
    assert!(!outcomes[0].is_error);
    assert_eq!(approver.tainted.load(Ordering::SeqCst), 1);

    conn.cancel().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_mcp_tool_never_widens_a_permission_mode() {
    // The MCP baseline is `Ask`; only `BypassPermissions` (an explicit user
    // choice) short-circuits it, and `Plan` forbids it outright. A server has no
    // way to change that: its `annotations` are never read as a decision.
    for (mode, expected) in [
        (PermissionMode::Default, Resolved::Ask),
        (PermissionMode::AcceptEdits, Resolved::Ask),
        (PermissionMode::DontAsk, Resolved::Allow),
        (PermissionMode::Plan, Resolved::Deny),
        (PermissionMode::BypassPermissions, Resolved::Allow),
    ] {
        let resolved = resolve_permission(
            mode,
            agent_tools::PermissionDecision::Ask,
            false, // is_read_only
            true,  // is_sensitive
            true,  // is_taint_sensitive
            false, // taint_recent
        );
        assert_eq!(resolved, expected, "mode {mode:?}");
    }
}

/// Minimal MCP server: `initialize`, `tools/list`, `tools/call`. Same crude
/// line-based JSON handling as `stdio_lifecycle.rs`, kept dependency-free.
const FIXTURE_SRC: &str = r#"
use std::io::{self, BufRead, Write};

fn id_value(line: &str) -> String {
    let Some(id_pos) = line.find("\"id\"") else {
        return "null".to_string();
    };
    let Some(colon_pos) = line[id_pos..].find(':') else {
        return "null".to_string();
    };
    let rest = line[id_pos + colon_pos + 1..].trim_start();
    let end = rest.find(|c| c == ',' || c == '}').unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

/// Value of the `text` argument, or `(none)`.
fn text_arg(line: &str) -> String {
    let Some(pos) = line.find("\"text\":\"") else {
        return "(none)".to_string();
    };
    let rest = &line[pos + 8..];
    let end = rest.find('"').unwrap_or(rest.len());
    rest[..end].to_string()
}

fn send(id: &str, result: &str) {
    let mut out = io::stdout();
    writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}", id, result).unwrap();
    out.flush().unwrap();
}

fn tool(name: &str, extra: &str) -> String {
    format!(
        "{{\"name\":\"{}\",\"description\":\"fixture {}\",\"inputSchema\":{{\"type\":\"object\",\"properties\":{{\"text\":{{\"type\":\"string\"}},\"loud\":{{\"type\":\"boolean\"}}}},\"required\":[\"text\"]{}}}}}",
        name, name, extra
    )
}

fn main() {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let id = id_value(&line);
        if line.contains("\"initialize\"") {
            send(
                &id,
                "{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{\"listChanged\":false}},\"serverInfo\":{\"name\":\"pyxis-mcp-call-fixture\",\"version\":\"0.1.0\"}}",
            );
        } else if line.contains("\"tools/list\"") {
            let tools = format!(
                "{{\"tools\":[{},{},{},{}]}}",
                tool("echo", ""),
                tool("boom", ""),
                tool("picture", ""),
                tool("pid", "")
            );
            send(&id, &tools);
        } else if line.contains("\"tools/call\"") {
            if line.contains("\"name\":\"echo\"") {
                // An argument left at null by the model must never arrive here.
                let extra = if line.contains("null") { " [null leaked]" } else { "" };
                send(
                    &id,
                    &format!(
                        "{{\"content\":[{{\"type\":\"text\",\"text\":\"echo: {}{}\"}}]}}",
                        text_arg(&line),
                        extra
                    ),
                );
            } else if line.contains("\"name\":\"boom\"") {
                send(
                    &id,
                    "{\"content\":[{\"type\":\"text\",\"text\":\"tool failed\"}],\"isError\":true}",
                );
            } else if line.contains("\"name\":\"picture\"") {
                let data = "A".repeat(512);
                send(
                    &id,
                    &format!(
                        "{{\"content\":[{{\"type\":\"image\",\"mimeType\":\"image/png\",\"data\":\"{}\"}}]}}",
                        data
                    ),
                );
            } else if line.contains("\"name\":\"pid\"") {
                send(
                    &id,
                    &format!(
                        "{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}",
                        std::process::id()
                    ),
                );
            } else if line.contains("\"name\":\"die\"") {
                std::process::exit(3);
            }
            // `stall`: no answer at all, on purpose.
        }
    }
}
"#;
