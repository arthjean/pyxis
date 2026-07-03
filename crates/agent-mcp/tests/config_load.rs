//! Tests d'intégration du chargement de `.mcp.json` (API publique
//! `McpConfigFile::load`). Vérifie la tolérance aux serveurs non-stdio.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use agent_mcp::{McpConfigFile, McpConfigIssueKind, McpConfigOrigin};

/// Dossier temporaire unique pour un test (sans dépendance externe). Nettoyé en
/// début ET fin de test.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pyxis-mcp-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn absent_file_is_empty_config() {
    let dir = temp_dir("absent");
    let cfg = McpConfigFile::load(&dir).unwrap();
    assert!(cfg.servers.is_empty());
    assert_eq!(cfg.skipped, 0);
    assert!(cfg.issues.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parses_stdio_server_with_args_and_env() {
    let dir = temp_dir("stdio");
    std::fs::write(
        dir.join(".mcp.json"),
        r#"{ "mcpServers": {
            "filesystem": {
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
                "env": { "FOO": "bar" }
            }
        } }"#,
    )
    .unwrap();
    let cfg = McpConfigFile::load(&dir).unwrap();
    assert_eq!(cfg.servers.len(), 1);
    let fs = cfg.servers.get("filesystem").unwrap();
    assert_eq!(fs.command, "npx");
    assert_eq!(fs.args.len(), 3);
    assert_eq!(fs.env.get("FOO").map(String::as_str), Some("bar"));
    assert_eq!(fs.source.origin, McpConfigOrigin::Workspace);
    assert_eq!(cfg.skipped, 0);
    assert!(cfg.issues.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remote_server_without_command_is_skipped_not_fatal() {
    let dir = temp_dir("remote");
    std::fs::write(
        dir.join(".mcp.json"),
        r#"{ "mcpServers": {
            "stdio-ok": { "command": "echo", "args": ["hi"] },
            "remote": { "type": "http", "url": "https://example.com/mcp" }
        } }"#,
    )
    .unwrap();
    let cfg = McpConfigFile::load(&dir).unwrap();
    // Le serveur stdio est gardé, le remote (sans `command`) est ignoré sans erreur.
    assert_eq!(cfg.servers.len(), 1);
    assert!(cfg.servers.contains_key("stdio-ok"));
    assert_eq!(cfg.skipped, 1);
    assert!(matches!(
        cfg.issues[0].kind,
        McpConfigIssueKind::UnsupportedTransport
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn disabled_server_is_skipped_with_issue() {
    let dir = temp_dir("disabled");
    std::fs::write(
        dir.join(".mcp.json"),
        r#"{ "mcpServers": {
            "off": { "command": "echo", "disabled": true }
        } }"#,
    )
    .unwrap();
    let cfg = McpConfigFile::load(&dir).unwrap();
    assert!(cfg.servers.is_empty());
    assert_eq!(cfg.skipped, 1);
    assert!(matches!(cfg.issues[0].kind, McpConfigIssueKind::Disabled));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_command_is_skipped_with_issue() {
    let dir = temp_dir("empty-command");
    std::fs::write(
        dir.join(".mcp.json"),
        r#"{ "mcpServers": {
            "empty": { "command": "   " }
        } }"#,
    )
    .unwrap();
    let cfg = McpConfigFile::load(&dir).unwrap();
    assert!(cfg.servers.is_empty());
    assert_eq!(cfg.skipped, 1);
    assert!(matches!(
        cfg.issues[0].kind,
        McpConfigIssueKind::EmptyCommand
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_stdio_entry_is_skipped_with_issue() {
    let dir = temp_dir("invalid-entry");
    std::fs::write(
        dir.join(".mcp.json"),
        r#"{ "mcpServers": {
            "bad": { "command": ["not", "a", "string"] }
        } }"#,
    )
    .unwrap();
    let cfg = McpConfigFile::load(&dir).unwrap();
    assert!(cfg.servers.is_empty());
    assert_eq!(cfg.skipped, 1);
    assert!(matches!(
        cfg.issues[0].kind,
        McpConfigIssueKind::InvalidEntry(_)
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn malformed_json_is_an_error() {
    let dir = temp_dir("bad");
    std::fs::write(dir.join(".mcp.json"), "{ not json").unwrap();
    assert!(McpConfigFile::load(&dir).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_claude_extracts_user_scope_mcpservers_only() {
    let dir = temp_dir("claude");
    let path = dir.join("claude.json");
    // Forme réelle de ~/.claude.json : plein de clés annexes + mcpServers user-scope
    // + un scope projet imbriqué (qu'on ne doit PAS lire).
    std::fs::write(
        &path,
        r#"{
            "numStartups": 42,
            "theme": "dark",
            "mcpServers": {
                "exa": { "command": "bunx", "args": ["-y", "exa-mcp-server"], "env": { "EXA_API_KEY": "secret" } },
                "remote": { "type": "http", "url": "https://example.com/mcp" }
            },
            "projects": { "/home/x": { "mcpServers": { "neon": { "command": "neon" } } } }
        }"#,
    )
    .unwrap();
    let cfg = McpConfigFile::load_claude(&path).unwrap();
    // user-scope uniquement : exa gardé, remote (sans command) skippé, neon
    // (project-scope) ignoré, clés annexes ignorées.
    assert_eq!(cfg.servers.len(), 1);
    assert!(cfg.servers.contains_key("exa"));
    assert!(!cfg.servers.contains_key("neon"));
    assert_eq!(
        cfg.servers.get("exa").unwrap().source.origin,
        McpConfigOrigin::ClaudeUser
    );
    assert_eq!(cfg.skipped, 1);
    assert!(matches!(
        cfg.issues[0].kind,
        McpConfigIssueKind::UnsupportedTransport
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Smoke local : vérifie que le vrai `~/.claude.json` de la machine parse et
/// expose des serveurs. Ignoré par défaut (dépend de l'environnement) ; lancer
/// avec `cargo test -p agent-mcp -- --ignored --nocapture`.
#[test]
#[ignore = "smoke local : dépend de ~/.claude.json réel"]
fn smoke_real_claude_json() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let path = std::path::Path::new(&home).join(".claude.json");
    if !path.exists() {
        return;
    }
    let cfg = McpConfigFile::load_claude(&path).expect("~/.claude.json doit parser");
    eprintln!(
        "serveurs MCP stdio découverts: {:?} (skipped remote: {})",
        cfg.servers.keys().collect::<Vec<_>>(),
        cfg.skipped
    );
}
