//! Parsing of `.mcp.json` (Claude Code compatible format): the `stdio` and
//! remote `http` transports, plus the per-server tool policy (exposure filter and
//! approval level). Invalid or disabled entries are kept as diagnostics instead of
//! disappearing silently.
//!
//! Two rules make this file a security boundary rather than a parser:
//!
//! 1. **A workspace entry never widens.** An approval level declared by a file the
//!    workspace controls is dropped with a diagnostic, exactly like the security
//!    keys of `settings.rs`. Opening a repository must not be enough to
//!    auto-approve a remote call.
//! 2. **A shadowing entry only narrows.** When an entry hides a lower-priority
//!    one, the effective policy is the composition of the two: the allow-lists
//!    intersect, the deny-lists add up, and the strictest approval wins.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::McpError;

/// Origin of an MCP entry. The workspace takes priority over the user configuration.
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

    /// Is this origin a file the workspace controls? Such a file can restrict,
    /// never widen (hard constraint of the PRD, FR-18).
    pub fn is_workspace_controlled(&self) -> bool {
        matches!(self, Self::Workspace)
    }
}

/// Concrete source of an MCP entry, used by the trust UI and the diagnostics.
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

/// Reason why an MCP entry is not enabled as is, or was altered on load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpConfigIssueKind {
    Disabled,
    UnsupportedTransport,
    InvalidEntry(String),
    EmptyCommand,
    Shadowed {
        kept_source: McpConfigSource,
    },
    /// Remote URL refused before any connection (US-013 AC6).
    InsecureUrl(String),
    /// An approval level declared by a workspace-controlled file: dropped
    /// (US-015 AC4).
    ApprovalIgnored,
    /// A shadowing entry was narrowed down to what the shadowed one allowed
    /// (US-014 AC4).
    FilterNarrowed {
        kept_source: McpConfigSource,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigIssue {
    pub server: String,
    pub source: McpConfigSource,
    pub kind: McpConfigIssueKind,
}

impl McpConfigIssue {
    pub fn summary(&self) -> String {
        let server = &self.server;
        let origin = self.source.short_label();
        match &self.kind {
            McpConfigIssueKind::Disabled => format!("{server} ({origin}) disabled"),
            McpConfigIssueKind::UnsupportedTransport => {
                format!("{server} ({origin}) unsupported transport ignored")
            }
            McpConfigIssueKind::InvalidEntry(err) => {
                format!("{server} ({origin}) invalid entry: {err}")
            }
            McpConfigIssueKind::EmptyCommand => {
                format!("{server} ({origin}) empty command ignored")
            }
            McpConfigIssueKind::Shadowed { kept_source } => format!(
                "{server} ({origin}) shadowed by {}",
                kept_source.short_label()
            ),
            McpConfigIssueKind::InsecureUrl(url) => format!(
                "{server} ({origin}) refused: {url} is not https (only a loopback host may be plain http)"
            ),
            McpConfigIssueKind::ApprovalIgnored => format!(
                "{server} ({origin}) tool approval ignored: a workspace file cannot auto-approve"
            ),
            McpConfigIssueKind::FilterNarrowed { kept_source } => format!(
                "{server} ({origin}) tool policy narrowed down to the {} one: a shadowing entry cannot widen",
                kept_source.short_label()
            ),
        }
    }
}

/// Approval level of an MCP tool (US-015). Adapted from Codex
/// (`default_tools_approval_mode` per server plus a per-tool table): two states
/// only, because the permission mode and the taint defense already sit above this
/// one and can always tighten it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum McpApproval {
    /// Human confirmation requested, current behavior.
    #[default]
    Ask,
    /// No confirmation for this tool: the baseline the pipeline starts from. The
    /// permission mode, the hooks and the taint defense still apply.
    Allow,
}

impl McpApproval {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ask" => Some(Self::Ask),
            "allow" => Some(Self::Allow),
            _ => None,
        }
    }

    /// The stricter of two levels. `Ask` always wins: composing policies can only
    /// tighten.
    fn strictest(self, other: Self) -> Self {
        if self == Self::Allow && other == Self::Allow {
            Self::Allow
        } else {
            Self::Ask
        }
    }
}

/// Per-server tool policy: which tools reach the model (US-014) and at which
/// approval level (US-015).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpToolPolicy {
    /// Allow-list. `None` = no allow-list, every tool passes this first step.
    pub enabled: Option<BTreeSet<String>>,
    /// Deny-list, applied AFTER the allow-list (order taken from Codex).
    pub disabled: BTreeSet<String>,
    pub default_approval: McpApproval,
    pub per_tool_approval: BTreeMap<String, McpApproval>,
}

impl McpToolPolicy {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Does this policy expose `tool`? Allow-list first, deny-list second: a name
    /// present in both is denied.
    pub fn exposes(&self, tool: &str) -> bool {
        if let Some(enabled) = &self.enabled
            && !enabled.contains(tool)
        {
            return false;
        }
        !self.disabled.contains(tool)
    }

    /// Approval level for one tool: the per-tool setting wins over the server
    /// default (US-015 AC2).
    pub fn approval_for(&self, tool: &str) -> McpApproval {
        self.per_tool_approval
            .get(tool)
            .copied()
            .unwrap_or(self.default_approval)
    }

    /// Names this policy mentions that the server does not expose (US-014 AC3).
    pub fn unknown_names<'a>(&'a self, available: &BTreeSet<&str>) -> Vec<&'a str> {
        self.enabled
            .iter()
            .flatten()
            .chain(self.disabled.iter())
            .chain(self.per_tool_approval.keys())
            .map(String::as_str)
            .filter(|name| !available.contains(name))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Drops everything that could widen: used on a workspace-controlled file.
    /// Restrictions are kept, approvals fall back to `Ask`.
    fn without_widening(&self) -> Self {
        Self {
            enabled: self.enabled.clone(),
            disabled: self.disabled.clone(),
            default_approval: McpApproval::Ask,
            per_tool_approval: BTreeMap::new(),
        }
    }

    fn widens(&self) -> bool {
        self.default_approval == McpApproval::Allow
            || self
                .per_tool_approval
                .values()
                .any(|level| *level == McpApproval::Allow)
    }

    /// Restricts this policy to what `outer` already allowed (US-014 AC4): the
    /// allow-lists intersect, the deny-lists add up, and the strictest approval
    /// wins on each tool.
    #[must_use]
    fn narrowed_by(&self, outer: &Self) -> Self {
        let enabled = match (&self.enabled, &outer.enabled) {
            (Some(mine), Some(theirs)) => Some(mine.intersection(theirs).cloned().collect()),
            (Some(mine), None) => Some(mine.clone()),
            (None, Some(theirs)) => Some(theirs.clone()),
            (None, None) => None,
        };
        let disabled = self.disabled.union(&outer.disabled).cloned().collect();
        let mut per_tool_approval = BTreeMap::new();
        for tool in self
            .per_tool_approval
            .keys()
            .chain(outer.per_tool_approval.keys())
        {
            let level = self.approval_for(tool).strictest(outer.approval_for(tool));
            per_tool_approval.insert(tool.clone(), level);
        }
        Self {
            enabled,
            disabled,
            default_approval: self.default_approval.strictest(outer.default_approval),
            per_tool_approval,
        }
    }
}

/// How to reach a server. The stdio variant spawns a subprocess; the HTTP one
/// talks to a remote party (US-013).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        /// NAME of the environment variable holding the bearer token, never the
        /// token itself: nothing readable ever enters this struct, hence neither
        /// the logs nor the transcript (US-013 AC3).
        bearer_token_env_var: Option<String>,
    },
}

impl McpTransport {
    pub fn short_label(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
        }
    }

    /// Env variables of a stdio server (empty for a remote server).
    pub fn env(&self) -> &BTreeMap<String, String> {
        match self {
            Self::Stdio { env, .. } => env,
            Self::Http { .. } => {
                static EMPTY: std::sync::OnceLock<BTreeMap<String, String>> =
                    std::sync::OnceLock::new();
                EMPTY.get_or_init(BTreeMap::new)
            }
        }
    }

    /// One-line description shown by the trust prompt: the command to spawn, or
    /// the endpoint to talk to.
    pub fn target(&self) -> String {
        match self {
            Self::Stdio { command, args, .. } => {
                if args.is_empty() {
                    command.clone()
                } else {
                    format!("{command} {}", args.join(" "))
                }
            }
            Self::Http { url, .. } => url.clone(),
        }
    }
}

/// Configuration of one MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub transport: McpTransport,
    pub tools: McpToolPolicy,
    pub source: McpConfigSource,
    pub shadows_lower_priority: bool,
}

impl McpServerConfig {
    /// Test/manual helper: a stdio server with no policy.
    pub fn stdio(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            transport: McpTransport::Stdio {
                command: command.into(),
                args,
                env: BTreeMap::new(),
            },
            tools: McpToolPolicy::default(),
            source: McpConfigSource::default(),
            shadows_lower_priority: false,
        }
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        self.transport.env()
    }
}

/// Unknown keys are ignored on purpose: `~/.claude.json` carries entries written
/// for other clients, and a key Pyxis does not know must not disable a server that
/// works everywhere else.
#[derive(Debug, Deserialize)]
struct RawMcpServerConfig {
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    #[serde(rename = "bearerTokenEnvVar")]
    bearer_token_env_var: Option<String>,
    #[serde(rename = "enabledTools")]
    enabled_tools: Option<BTreeSet<String>>,
    #[serde(rename = "disabledTools", default)]
    disabled_tools: BTreeSet<String>,
    #[serde(rename = "toolsApproval")]
    tools_approval: Option<RawToolsApproval>,
    /// Read by the caller before this struct; accepted here so that
    /// `deny_unknown_fields` does not reject the entry.
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Deserialize)]
struct RawToolsApproval {
    default: Option<String>,
    #[serde(default)]
    tools: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawConfigFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, serde_json::Value>,
}

/// Resolved content of `.mcp.json`: usable servers and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct McpConfigFile {
    pub servers: BTreeMap<String, McpServerConfig>,
    pub skipped: usize,
    pub issues: Vec<McpConfigIssue>,
}

impl McpConfigFile {
    /// Loads `<dir>/.mcp.json` (workspace MCP config). Missing file: empty config.
    pub fn load(dir: &Path) -> Result<Self, McpError> {
        Self::load_file(&dir.join(".mcp.json"), McpConfigOrigin::Workspace)
    }

    /// Loads the user-scope `mcpServers` of a Claude Code file (`~/.claude.json`).
    pub fn load_claude(path: &Path) -> Result<Self, McpError> {
        Self::load_file(path, McpConfigOrigin::ClaudeUser)
    }

    /// Merges `lower` under `self`. On a collision the high-priority config wins,
    /// the low-priority entry is recorded as shadowed, and the kept policy is
    /// narrowed down to what the shadowed one allowed (US-014 AC4).
    #[must_use]
    pub fn merge_under(mut self, lower: McpConfigFile) -> Self {
        for (name, cfg) in lower.servers {
            if let Some(existing) = self.servers.get_mut(&name) {
                existing.shadows_lower_priority = true;
                let narrowed = existing.tools.narrowed_by(&cfg.tools);
                if narrowed != existing.tools {
                    self.issues.push(McpConfigIssue {
                        server: name.clone(),
                        source: existing.source.clone(),
                        kind: McpConfigIssueKind::FilterNarrowed {
                            kept_source: cfg.source.clone(),
                        },
                    });
                    existing.tools = narrowed;
                }
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
        let mut skipped = 0;
        for (name, value) in file.mcp_servers {
            match parse_server(&name, value, &source) {
                Ok((cfg, warnings)) => {
                    servers.insert(name, cfg);
                    issues.extend(warnings);
                }
                Err(issue) => {
                    skipped += 1;
                    issues.push(issue);
                }
            }
        }
        Ok(Self {
            servers,
            skipped,
            issues,
        })
    }
}

/// Parses one entry. `Ok` carries the diagnostics of an entry that was accepted
/// but altered (a dropped approval); `Err` carries the single reason it was refused.
fn parse_server(
    name: &str,
    value: serde_json::Value,
    source: &McpConfigSource,
) -> Result<(McpServerConfig, Vec<McpConfigIssue>), McpConfigIssue> {
    let raw = serde_json::from_value::<RawMcpServerConfig>(value).map_err(|e| {
        issue(
            name,
            source,
            McpConfigIssueKind::InvalidEntry(e.to_string()),
        )
    })?;
    if raw.disabled {
        return Err(issue(name, source, McpConfigIssueKind::Disabled));
    }
    let transport = parse_transport(name, &raw, source)?;
    let mut warnings = Vec::new();
    let mut tools = parse_policy(name, &raw, source)?;
    // FR-18: a file the workspace controls may restrict, never widen.
    if source.origin.is_workspace_controlled() && tools.widens() {
        tools = tools.without_widening();
        warnings.push(issue(name, source, McpConfigIssueKind::ApprovalIgnored));
    }
    Ok((
        McpServerConfig {
            transport,
            tools,
            source: source.clone(),
            shadows_lower_priority: false,
        },
        warnings,
    ))
}

fn parse_transport(
    name: &str,
    raw: &RawMcpServerConfig,
    source: &McpConfigSource,
) -> Result<McpTransport, McpConfigIssue> {
    let kind = raw.kind.as_deref().map(str::trim);
    // The declared type wins; without one the presence of `command` or `url`
    // decides, which is how the Claude Code files in the wild are written.
    let is_http = match kind {
        Some(k) if k.eq_ignore_ascii_case("http") || k.eq_ignore_ascii_case("streamable-http") => {
            true
        }
        Some(k) if k.eq_ignore_ascii_case("stdio") => false,
        // `sse` (the superseded transport) is not implemented: named as
        // unsupported rather than silently treated as Streamable HTTP.
        Some(_) => {
            return Err(issue(
                name,
                source,
                McpConfigIssueKind::UnsupportedTransport,
            ));
        }
        None => raw.command.is_none() && raw.url.is_some(),
    };
    if is_http {
        let Some(url) = raw.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
            return Err(issue(
                name,
                source,
                McpConfigIssueKind::UnsupportedTransport,
            ));
        };
        validate_url(name, url, source)?;
        return Ok(McpTransport::Http {
            url: url.to_string(),
            bearer_token_env_var: raw
                .bearer_token_env_var
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string),
        });
    }
    let Some(command) = raw.command.as_deref() else {
        return Err(issue(
            name,
            source,
            McpConfigIssueKind::UnsupportedTransport,
        ));
    };
    if command.trim().is_empty() {
        return Err(issue(name, source, McpConfigIssueKind::EmptyCommand));
    }
    Ok(McpTransport::Stdio {
        command: command.to_string(),
        args: raw.args.clone(),
        env: raw.env.clone(),
    })
}

/// US-013 AC6: a remote endpoint must be encrypted. Plain `http` is accepted for a
/// loopback host only, where there is no network to eavesdrop on and where every
/// local MCP server in development lives.
fn validate_url(name: &str, url: &str, source: &McpConfigSource) -> Result<(), McpConfigIssue> {
    let parsed = reqwest::Url::parse(url).map_err(|e| {
        issue(
            name,
            source,
            McpConfigIssueKind::InvalidEntry(format!("invalid url: {e}")),
        )
    })?;
    let secure = match parsed.scheme() {
        "https" => true,
        "http" => is_loopback_host(&parsed),
        _ => false,
    };
    if secure {
        Ok(())
    } else {
        Err(issue(
            name,
            source,
            McpConfigIssueKind::InsecureUrl(url.to_string()),
        ))
    }
}

fn is_loopback_host(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    // `host_str` serializes an IPv6 literal with its brackets.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn parse_policy(
    name: &str,
    raw: &RawMcpServerConfig,
    source: &McpConfigSource,
) -> Result<McpToolPolicy, McpConfigIssue> {
    let mut policy = McpToolPolicy {
        enabled: raw.enabled_tools.clone(),
        disabled: raw.disabled_tools.clone(),
        ..McpToolPolicy::default()
    };
    let Some(approval) = &raw.tools_approval else {
        return Ok(policy);
    };
    if let Some(default) = approval.default.as_deref() {
        policy.default_approval = McpApproval::parse(default).ok_or_else(|| {
            issue(
                name,
                source,
                McpConfigIssueKind::InvalidEntry(format!(
                    "toolsApproval.default: expected \"ask\" or \"allow\", got \"{default}\""
                )),
            )
        })?;
    }
    for (tool, level) in &approval.tools {
        let level = McpApproval::parse(level).ok_or_else(|| {
            issue(
                name,
                source,
                McpConfigIssueKind::InvalidEntry(format!(
                    "toolsApproval.tools.{tool}: expected \"ask\" or \"allow\", got \"{level}\""
                )),
            )
        })?;
        policy.per_tool_approval.insert(tool.clone(), level);
    }
    Ok(policy)
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

    fn parse(name: &str, value: serde_json::Value, origin: McpConfigOrigin) -> McpServerConfig {
        let source = McpConfigSource::new(origin, "");
        parse_server(name, value, &source)
            .map(|(cfg, _)| cfg)
            .map_err(|issue| issue.summary())
            .expect("entry must parse")
    }

    fn refusal(value: serde_json::Value, origin: McpConfigOrigin) -> McpConfigIssueKind {
        let source = McpConfigSource::new(origin, "");
        parse_server("srv", value, &source)
            .map(|(cfg, _)| cfg)
            .expect_err("entry must be refused")
            .kind
    }

    fn server(command: &str, origin: McpConfigOrigin) -> McpServerConfig {
        McpServerConfig {
            source: McpConfigSource::new(origin, ""),
            ..McpServerConfig::stdio(command, Vec::new())
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
        assert_eq!(merged.servers["a"].transport.target(), "high");
        assert!(merged.servers["a"].shadows_lower_priority);
        assert_eq!(merged.servers["b"].transport.target(), "low-b");
        assert_eq!(merged.skipped, 2);
        assert_eq!(merged.issues.len(), 1);
        assert!(matches!(
            merged.issues[0].kind,
            McpConfigIssueKind::Shadowed { .. }
        ));
    }

    #[test]
    fn a_shadowing_entry_cannot_widen_the_shadowed_filter() {
        let mut restrictive = server("high", McpConfigOrigin::Workspace);
        restrictive.tools.enabled = Some(BTreeSet::from(["read".into(), "write".into()]));
        let mut high = BTreeMap::new();
        high.insert("a".to_string(), restrictive);
        let high = McpConfigFile {
            servers: high,
            skipped: 0,
            issues: Vec::new(),
        };

        let mut narrow = server("low", McpConfigOrigin::ClaudeUser);
        narrow.tools.enabled = Some(BTreeSet::from(["read".into()]));
        narrow.tools.disabled = BTreeSet::from(["danger".into()]);
        let mut low = BTreeMap::new();
        low.insert("a".to_string(), narrow);
        let low = McpConfigFile {
            servers: low,
            skipped: 0,
            issues: Vec::new(),
        };

        let merged = high.merge_under(low);
        let policy = &merged.servers["a"].tools;
        // The intersection: `write`, which only the shadowing entry allowed, is gone.
        assert_eq!(policy.enabled, Some(BTreeSet::from(["read".to_string()])));
        assert!(policy.disabled.contains("danger"));
        assert!(policy.exposes("read"));
        assert!(!policy.exposes("write"));
        assert!(
            merged
                .issues
                .iter()
                .any(|i| matches!(i.kind, McpConfigIssueKind::FilterNarrowed { .. }))
        );
    }

    #[test]
    fn stdio_entry_is_parsed_without_a_declared_type() {
        let cfg = parse(
            "srv",
            serde_json::json!({"command": "node", "args": ["server.js"], "env": {"A": "1"}}),
            McpConfigOrigin::ClaudeUser,
        );
        assert_eq!(cfg.transport.short_label(), "stdio");
        assert_eq!(cfg.transport.target(), "node server.js");
        assert_eq!(cfg.env().get("A").map(String::as_str), Some("1"));
    }

    #[test]
    fn http_entry_is_parsed_and_keeps_only_the_env_var_name() {
        let cfg = parse(
            "srv",
            serde_json::json!({
                "type": "http",
                "url": "https://mcp.example.com/mcp",
                "bearerTokenEnvVar": "EXAMPLE_TOKEN"
            }),
            McpConfigOrigin::ClaudeUser,
        );
        assert_eq!(
            cfg.transport,
            McpTransport::Http {
                url: "https://mcp.example.com/mcp".to_string(),
                bearer_token_env_var: Some("EXAMPLE_TOKEN".to_string()),
            }
        );
        // US-013 AC3: nothing readable is stored, so nothing can leak through Debug.
        assert!(!format!("{cfg:?}").contains("secret"));
    }

    #[test]
    fn plain_http_is_refused_unless_it_is_loopback() {
        assert!(matches!(
            refusal(
                serde_json::json!({"type": "http", "url": "http://mcp.example.com/mcp"}),
                McpConfigOrigin::ClaudeUser
            ),
            McpConfigIssueKind::InsecureUrl(_)
        ));
        for url in [
            "http://localhost:8080/mcp",
            "http://127.0.0.1:8080/mcp",
            "http://[::1]:8080/mcp",
        ] {
            let cfg = parse(
                "srv",
                serde_json::json!({"type": "http", "url": url}),
                McpConfigOrigin::ClaudeUser,
            );
            assert_eq!(cfg.transport.short_label(), "http", "{url}");
        }
    }

    #[test]
    fn a_lookalike_loopback_domain_is_still_refused() {
        // `localhost.evil.com` resolves wherever its owner wants: only the exact
        // host is loopback.
        assert!(matches!(
            refusal(
                serde_json::json!({"type": "http", "url": "http://localhost.evil.com/mcp"}),
                McpConfigOrigin::ClaudeUser
            ),
            McpConfigIssueKind::InsecureUrl(_)
        ));
    }

    #[test]
    fn unsupported_and_empty_transports_are_named() {
        assert_eq!(
            refusal(
                serde_json::json!({"type": "sse", "url": "https://x.example.com/sse"}),
                McpConfigOrigin::ClaudeUser
            ),
            McpConfigIssueKind::UnsupportedTransport
        );
        assert_eq!(
            refusal(serde_json::json!({}), McpConfigOrigin::ClaudeUser),
            McpConfigIssueKind::UnsupportedTransport
        );
        assert_eq!(
            refusal(
                serde_json::json!({"command": "   "}),
                McpConfigOrigin::ClaudeUser
            ),
            McpConfigIssueKind::EmptyCommand
        );
        assert_eq!(
            refusal(
                serde_json::json!({"command": "x", "disabled": true}),
                McpConfigOrigin::ClaudeUser
            ),
            McpConfigIssueKind::Disabled
        );
    }

    #[test]
    fn allow_list_runs_before_the_deny_list() {
        let cfg = parse(
            "srv",
            serde_json::json!({
                "command": "x",
                "enabledTools": ["read", "write"],
                "disabledTools": ["write", "delete"]
            }),
            McpConfigOrigin::ClaudeUser,
        );
        assert!(cfg.tools.exposes("read"));
        // Listed in both: the deny-list applies last, hence wins.
        assert!(!cfg.tools.exposes("write"));
        assert!(!cfg.tools.exposes("delete"));
        assert!(!cfg.tools.exposes("other"));
    }

    #[test]
    fn approval_is_per_tool_then_per_server() {
        let cfg = parse(
            "srv",
            serde_json::json!({
                "command": "x",
                "toolsApproval": {"default": "allow", "tools": {"delete": "ask"}}
            }),
            McpConfigOrigin::ClaudeUser,
        );
        assert_eq!(cfg.tools.approval_for("read"), McpApproval::Allow);
        assert_eq!(cfg.tools.approval_for("delete"), McpApproval::Ask);
    }

    #[test]
    fn a_workspace_file_cannot_auto_approve_but_can_still_restrict() {
        let source = McpConfigSource::new(McpConfigOrigin::Workspace, "");
        let (cfg, warnings) = parse_server(
            "srv",
            serde_json::json!({
                "command": "x",
                "disabledTools": ["delete"],
                "toolsApproval": {"default": "allow", "tools": {"read": "allow"}}
            }),
            &source,
        )
        .expect("entry stays usable");
        assert_eq!(cfg.tools.approval_for("read"), McpApproval::Ask);
        assert_eq!(cfg.tools.approval_for("other"), McpApproval::Ask);
        // The restriction survives: only the widening is dropped.
        assert!(!cfg.tools.exposes("delete"));
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].kind, McpConfigIssueKind::ApprovalIgnored);
    }

    #[test]
    fn an_invalid_approval_level_names_the_key_and_the_value() {
        let kind = refusal(
            serde_json::json!({"command": "x", "toolsApproval": {"default": "always"}}),
            McpConfigOrigin::ClaudeUser,
        );
        let McpConfigIssueKind::InvalidEntry(err) = kind else {
            unreachable!("expected an invalid entry, got {kind:?}")
        };
        assert!(err.contains("toolsApproval.default"), "{err}");
        assert!(err.contains("always"), "{err}");
    }

    #[test]
    fn listed_names_absent_from_the_server_are_reported() {
        let cfg = parse(
            "srv",
            serde_json::json!({
                "command": "x",
                "enabledTools": ["read", "ghost"],
                "toolsApproval": {"tools": {"phantom": "allow"}}
            }),
            McpConfigOrigin::ClaudeUser,
        );
        let available = BTreeSet::from(["read"]);
        assert_eq!(
            cfg.tools.unknown_names(&available),
            vec!["ghost", "phantom"]
        );
    }
}
