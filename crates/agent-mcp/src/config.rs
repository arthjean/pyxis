//! Parsing de `.mcp.json` (format compatible Claude Code). Seul le transport
//! stdio est activable pour l'instant. Les entrées remote, invalides ou disabled
//! sont conservées comme diagnostics au lieu de disparaître silencieusement.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::McpError;

/// Origine d'une entrée MCP. Le workspace est prioritaire sur la config utilisateur.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum McpConfigOrigin {
    Workspace,
    ClaudeUser,
    #[default]
    Manual,
}

impl McpConfigOrigin {
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::ClaudeUser => "user",
            Self::Manual => "manual",
        }
    }
}

/// Source concrète d'une entrée MCP, utilisée par l'UI de trust et les diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpConfigSource {
    pub origin: McpConfigOrigin,
    pub path: PathBuf,
}

impl McpConfigSource {
    pub fn new(origin: McpConfigOrigin, path: impl Into<PathBuf>) -> Self {
        Self {
            origin,
            path: path.into(),
        }
    }

    pub fn short_label(&self) -> &'static str {
        self.origin.short_label()
    }

    pub fn display(&self) -> String {
        let origin = self.short_label();
        if self.path.as_os_str().is_empty() {
            origin.to_string()
        } else {
            format!("{origin}: {}", self.path.display())
        }
    }
}

/// Raison pour laquelle une entrée MCP n'est pas activée telle quelle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpConfigIssueKind {
    Disabled,
    UnsupportedTransport,
    InvalidEntry(String),
    EmptyCommand,
    Shadowed { kept_source: McpConfigSource },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigIssue {
    pub server: String,
    pub source: McpConfigSource,
    pub kind: McpConfigIssueKind,
}

impl McpConfigIssue {
    pub fn summary(&self) -> String {
        match &self.kind {
            McpConfigIssueKind::Disabled => {
                format!("{} ({}) disabled", self.server, self.source.short_label())
            }
            McpConfigIssueKind::UnsupportedTransport => format!(
                "{} ({}) transport non-stdio ignoré",
                self.server,
                self.source.short_label()
            ),
            McpConfigIssueKind::InvalidEntry(err) => format!(
                "{} ({}) entrée invalide: {err}",
                self.server,
                self.source.short_label()
            ),
            McpConfigIssueKind::EmptyCommand => format!(
                "{} ({}) commande vide ignorée",
                self.server,
                self.source.short_label()
            ),
            McpConfigIssueKind::Shadowed { kept_source } => format!(
                "{} ({}) masqué par {}",
                self.server,
                self.source.short_label(),
                kept_source.short_label()
            ),
        }
    }
}

/// Configuration d'un serveur MCP stdio: commande, arguments et variables d'env.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(skip)]
    pub source: McpConfigSource,
    #[serde(skip)]
    pub shadows_lower_priority: bool,
}

#[derive(Debug, Deserialize)]
struct RawMcpServerConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawConfigFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, serde_json::Value>,
}

/// Contenu résolu de `.mcp.json`: serveurs stdio exploitables et diagnostics.
#[derive(Debug, Clone, Default)]
pub struct McpConfigFile {
    pub servers: BTreeMap<String, McpServerConfig>,
    pub skipped: usize,
    pub issues: Vec<McpConfigIssue>,
}

impl McpConfigFile {
    /// Charge `<dir>/.mcp.json` (config MCP du workspace). Fichier absent: config vide.
    pub fn load(dir: &Path) -> Result<Self, McpError> {
        Self::load_file(&dir.join(".mcp.json"), McpConfigOrigin::Workspace)
    }

    /// Charge les `mcpServers` user-scope d'un fichier Claude Code (`~/.claude.json`).
    pub fn load_claude(path: &Path) -> Result<Self, McpError> {
        Self::load_file(path, McpConfigOrigin::ClaudeUser)
    }

    /// Fusionne `lower` sous `self`. En collision, la config haute priorité gagne
    /// et l'entrée basse priorité est enregistrée comme issue de shadowing.
    #[must_use]
    pub fn merge_under(mut self, lower: McpConfigFile) -> Self {
        for (name, cfg) in lower.servers {
            if let Some(existing) = self.servers.get_mut(&name) {
                existing.shadows_lower_priority = true;
                self.issues.push(McpConfigIssue {
                    server: name,
                    source: cfg.source,
                    kind: McpConfigIssueKind::Shadowed {
                        kept_source: existing.source.clone(),
                    },
                });
            } else {
                self.servers.insert(name, cfg);
            }
        }
        self.skipped += lower.skipped;
        self.issues.extend(lower.issues);
        self
    }

    fn load_file(path: &Path, origin: McpConfigOrigin) -> Result<Self, McpError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(McpError::Read(path.to_path_buf(), e)),
        };
        let file: RawConfigFile =
            serde_json::from_str(&raw).map_err(|e| McpError::Parse(path.to_path_buf(), e))?;

        let source = McpConfigSource::new(origin, path.to_path_buf());
        let mut servers = BTreeMap::new();
        let mut issues = Vec::new();
        for (name, value) in file.mcp_servers {
            match parse_server(&name, value, &source) {
                Ok(cfg) => {
                    servers.insert(name, cfg);
                }
                Err(issue) => issues.push(issue),
            }
        }
        Ok(Self {
            servers,
            skipped: issues.len(),
            issues,
        })
    }
}

fn parse_server(
    name: &str,
    value: serde_json::Value,
    source: &McpConfigSource,
) -> Result<McpServerConfig, McpConfigIssue> {
    if value
        .get("disabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Err(issue(name, source, McpConfigIssueKind::Disabled));
    }
    if value.get("command").is_none() {
        return Err(issue(
            name,
            source,
            McpConfigIssueKind::UnsupportedTransport,
        ));
    }
    let raw = serde_json::from_value::<RawMcpServerConfig>(value).map_err(|e| {
        issue(
            name,
            source,
            McpConfigIssueKind::InvalidEntry(e.to_string()),
        )
    })?;
    if raw.command.trim().is_empty() {
        return Err(issue(name, source, McpConfigIssueKind::EmptyCommand));
    }
    Ok(McpServerConfig {
        command: raw.command,
        args: raw.args,
        env: raw.env,
        source: source.clone(),
        shadows_lower_priority: false,
    })
}

fn issue(name: &str, source: &McpConfigSource, kind: McpConfigIssueKind) -> McpConfigIssue {
    McpConfigIssue {
        server: name.to_string(),
        source: source.clone(),
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(command: &str, origin: McpConfigOrigin) -> McpServerConfig {
        McpServerConfig {
            command: command.to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            source: McpConfigSource::new(origin, ""),
            shadows_lower_priority: false,
        }
    }

    #[test]
    fn merge_under_keeps_high_priority_on_collision() {
        let mut high = BTreeMap::new();
        high.insert("a".to_string(), server("high", McpConfigOrigin::Workspace));
        let high = McpConfigFile {
            servers: high,
            skipped: 0,
            issues: Vec::new(),
        };

        let mut low = BTreeMap::new();
        low.insert("a".to_string(), server("low", McpConfigOrigin::ClaudeUser));
        low.insert(
            "b".to_string(),
            server("low-b", McpConfigOrigin::ClaudeUser),
        );
        let low = McpConfigFile {
            servers: low,
            skipped: 2,
            issues: Vec::new(),
        };

        let merged = high.merge_under(low);
        assert_eq!(merged.servers.get("a").unwrap().command, "high");
        assert!(merged.servers.get("a").unwrap().shadows_lower_priority);
        assert_eq!(merged.servers.get("b").unwrap().command, "low-b");
        assert_eq!(merged.skipped, 2);
        assert_eq!(merged.issues.len(), 1);
        assert!(matches!(
            merged.issues[0].kind,
            McpConfigIssueKind::Shadowed { .. }
        ));
    }
}
