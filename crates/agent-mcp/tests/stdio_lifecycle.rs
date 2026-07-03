//! Smoke stdio réel: spawn d'un serveur MCP minimal, handshake, tools/list,
//! conservation des schémas et arrêt propre via `cancel`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use agent_mcp::{McpConnection, McpServerConfig};

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
    let cfg = McpServerConfig {
        command: exe.to_string_lossy().into_owned(),
        args: vec![closed.to_string_lossy().into_owned()],
        env: BTreeMap::new(),
        source: Default::default(),
        shadows_lower_priority: false,
    };

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
