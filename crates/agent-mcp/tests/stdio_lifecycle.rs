//! Real stdio smoke test: spawn of a minimal MCP server, handshake, tools/list,
//! schema preservation and clean shutdown through `cancel`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use agent_mcp::{McpConnection, McpServerConfig, McpServerPolicy};

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
        "mcp_fixture.exe"
    } else {
        "mcp_fixture"
    }
}

fn compile_fixture(dir: &Path) -> PathBuf {
    let src = dir.join("mcp_fixture.rs");
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

#[tokio::test]
async fn stdio_connect_lists_tools_and_cancel_closes_child() {
    let dir = temp_dir("stdio-lifecycle");
    let exe = compile_fixture(&dir);
    let closed = dir.join("closed.txt");
    let cfg = McpServerConfig::stdio(
        exe.to_string_lossy().into_owned(),
        vec![closed.to_string_lossy().into_owned()],
    );

    let conn = McpConnection::connect("fixture", &cfg).await.unwrap();
    let tools = conn.list_tools("fixture").await.unwrap();
    assert_eq!(tools.len(), 1);
    let tool = &tools[0];
    assert_eq!(tool.name, "fixture_echo");
    assert_eq!(tool.original_name, "fixture_echo");
    assert_eq!(tool.title.as_deref(), Some("Fixture Echo"));
    assert!(tool.description.contains("MCP fixture"));
    assert_eq!(tool.input_schema["type"], "object");
    assert_eq!(tool.input_schema["properties"]["text"]["type"], "string");
    assert_eq!(tool.output_schema.as_ref().unwrap()["type"], "object");
    assert!(tool.annotations_untrusted);

    conn.cancel().await;
    for _ in 0..40 {
        if closed.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        closed.exists(),
        "cancel doit fermer stdin et laisser le child sortir"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A server that dies during the handshake states why on stderr. Discarding it
/// used to leave the user with a bare timeout; the last words now travel with
/// the failure.
#[cfg(unix)]
#[tokio::test]
async fn a_failing_server_reports_its_last_stderr_lines() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("stdio-stderr");
    let script = dir.join("broken-server");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'FATAL: EXAMPLE_API_KEY is not set' >&2\nexit 1\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let cfg = McpServerConfig::stdio(script.to_string_lossy().into_owned(), Vec::new());
    let Err(err) = McpConnection::connect("broken", &cfg).await else {
        unreachable!("a server that exits during the handshake must fail")
    };
    let message = err.to_string();
    assert!(message.contains("broken"), "{message}");
    assert!(
        message.contains("EXAMPLE_API_KEY is not set"),
        "the server's own diagnosis must reach the user: {message}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A server that writes without ever sending a newline is cut off at the frame
/// bound instead of being allowed to allocate.
///
/// This is the primitive that sits UNDER every other bound in the crate: the
/// listing caps, the item caps and the output caps all assume a message that
/// already fits in memory. `rmcp`'s own stdio transport reads a frame into an
/// uncapped `Vec`, which is why the transport is built here rather than taken
/// from the SDK.
#[cfg(unix)]
#[tokio::test]
async fn an_unterminated_frame_is_cut_off_rather_than_allocated() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("stdio-frame");
    let script = dir.join("flooding-server");
    std::fs::write(
        &script,
        // 12 MiB on a single line, then silence: over the bound, and the process
        // stays alive so the failure can only come from the frame check.
        "#!/bin/sh\nhead -c 12582912 /dev/zero | tr '\\0' 'x'\nsleep 30\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let cfg = McpServerConfig {
        policy: McpServerPolicy {
            startup_timeout: Some(Duration::from_secs(20)),
            ..McpServerPolicy::default()
        },
        ..McpServerConfig::stdio(script.to_string_lossy().into_owned(), Vec::new())
    };
    let started = std::time::Instant::now();
    let Err(err) = McpConnection::connect("flood", &cfg).await else {
        unreachable!("a server that never frames a message cannot hand shake")
    };
    // The stream is cut on the bound, so the failure does NOT wait for the
    // startup deadline: that is exactly the difference the bound makes.
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the frame bound must end the stream, not let it run to the deadline"
    );
    assert!(
        !err.to_string().contains("timeout"),
        "expected a severed transport, got: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A per-server bound replaces the former fixed one, so a slow server can be
/// given room and a hung one still cannot hold the session.
#[cfg(unix)]
#[tokio::test]
async fn a_hung_server_expires_on_its_configured_bound() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("stdio-timeout");
    let script = dir.join("hung-server");
    std::fs::write(&script, "#!/bin/sh\nsleep 60\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let cfg = McpServerConfig {
        policy: McpServerPolicy {
            startup_timeout: Some(Duration::from_millis(300)),
            ..McpServerPolicy::default()
        },
        ..McpServerConfig::stdio(script.to_string_lossy().into_owned(), Vec::new())
    };
    let started = std::time::Instant::now();
    let Err(err) = McpConnection::connect("hung", &cfg).await else {
        unreachable!("a server that never handshakes must expire")
    };
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the configured bound is what applies"
    );
    assert!(err.to_string().contains("timeout"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

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
    let end = rest
        .find(|c| c == ',' || c == '}')
        .unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

fn send(id: &str, result: &str) {
    let mut out = io::stdout();
    writeln!(out, "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}", id, result).unwrap();
    out.flush().unwrap();
}

fn main() {
    let marker = std::env::args().nth(1).expect("marker path");
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let id = id_value(&line);
        if line.contains("\"initialize\"") {
            send(
                &id,
                "{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{\"tools\":{\"listChanged\":false}},\"serverInfo\":{\"name\":\"pyxis-mcp-fixture\",\"version\":\"0.1.0\"}}",
            );
        } else if line.contains("\"tools/list\"") {
            send(
                &id,
                "{\"tools\":[{\"name\":\"fixture_echo\",\"title\":\"Fixture Echo\",\"description\":\"MCP fixture echo tool\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}},\"required\":[\"text\"]},\"outputSchema\":{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}},\"required\":[\"text\"]},\"annotations\":{\"readOnlyHint\":true}}]}",
            );
        }
    }
    std::fs::write(marker, "closed").unwrap();
}
"#;
