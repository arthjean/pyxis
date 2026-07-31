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
use std::time::Duration;

use crate::error::McpError;

/// Default bound of a startup: spawn, `initialize`, remote retries and
/// `tools/list`, all under ONE deadline (see `client`).
///
/// A cold `npx -y <server>` routinely needs a few seconds before its first byte,
/// so a tight bound would make most of the ecosystem unusable; Codex uses 30s
/// (`codex-rs/codex-mcp/src/rmcp_client.rs`). It is also dead time the user pays
/// at every launch when a server is misconfigured, and the whole step is awaited
/// before the session opens, so the default is deliberately nearer the cold-start
/// cost than the ceiling. A server that genuinely needs longer says so with
/// `startupTimeoutMs`. The set is dialed concurrently, so this is the ceiling the
/// step adds, not a per-server sum.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Ceiling of a configured startup bound: a config file does not get to hold the
/// session hostage.
const MAX_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
/// Ceiling of a configured per-call bound.
const MAX_TOOL_TIMEOUT: Duration = Duration::from_secs(600);

mod issue;
mod parse;

pub use issue::{McpConfigIssue, McpConfigIssueKind};
use parse::{RawConfigFile, parse_server};

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
        /// NAMES of environment variables to forward from the parent process,
        /// never their values: the same contract as `bearer_token_env_var`,
        /// applied to stdio. Without it a stdio server needs its secrets written
        /// in clear inside the config file.
        env_vars: Vec<String>,
        /// Working directory of the subprocess. `None` keeps the one Pyxis runs in.
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        /// NAME of the environment variable holding the bearer token, never the
        /// token itself: nothing readable ever enters this struct, hence neither
        /// the logs nor the transcript (US-013 AC3).
        bearer_token_env_var: Option<String>,
        /// Literal headers sent on every request.
        http_headers: BTreeMap<String, String>,
        /// Headers whose VALUE is read from the named environment variable at
        /// connect time. Same rule as the bearer token: nothing readable is stored.
        env_http_headers: BTreeMap<String, String>,
        /// Parameters of the OAuth login for this server. The credential itself
        /// never lives here: it is in the OS secret store, keyed by server name.
        oauth: McpOAuthEntry,
    },
}

/// What a `/mcp <server> login` needs from the config. Everything is optional:
/// a server whose authorization server supports dynamic registration needs no
/// declaration at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpOAuthEntry {
    /// Pre-registered client id. Absent: dynamic registration (RFC 7591).
    pub client_id: Option<String>,
    /// Scopes to request. Empty: what the authorization server advertises.
    pub scopes: Vec<String>,
    /// RFC 8707 resource. Absent: the MCP url itself.
    pub resource: Option<String>,
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

    /// Names of the environment variables this server reads from the parent
    /// process: forwarded stdio variables, the bearer token, the env-sourced
    /// headers. Shown by the trust prompt so the human sees which credentials a
    /// connection would hand over.
    pub fn credential_env_names(&self) -> Vec<&str> {
        match self {
            Self::Stdio { env_vars, .. } => env_vars.iter().map(String::as_str).collect(),
            Self::Http {
                bearer_token_env_var,
                env_http_headers,
                ..
            } => bearer_token_env_var
                .as_deref()
                .into_iter()
                .chain(env_http_headers.values().map(String::as_str))
                .collect(),
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

/// What an entry declares ABOUT its tools and its bounds, as opposed to how to
/// reach the server.
///
/// Split from the transport because that is the honest boundary: the exposure
/// layer (`filter_tools`, `dyn_tools`) decides from this alone and has no
/// business seeing a command line or an endpoint. It also makes the shape
/// defaultable, so a caller with no entry to hand over says
/// `McpServerPolicy::default()` instead of fabricating a server config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpServerPolicy {
    /// Which tools reach the model, and at which approval level.
    pub tools: McpToolPolicy,
    /// Bound of the startup, when the entry declares one.
    pub startup_timeout: Option<Duration>,
    /// Bound of one tool call, when the entry declares one.
    pub tool_timeout: Option<Duration>,
    /// The session refuses to start headless when this server does not come up.
    /// Only honored on a server that is actually dialed at startup: a server
    /// behind the trust gate cannot make the session unstartable.
    pub required: bool,
    /// Lifts the serialization of this server's calls. A widening, hence
    /// dropped from a workspace-controlled file (FR-18).
    pub supports_parallel_tool_calls: bool,
}

impl McpServerPolicy {
    /// Effective startup bound: spawn, handshake, retries and `tools/list`
    /// together. One deadline covers the whole step (see `client`).
    pub fn startup_timeout(&self) -> Duration {
        self.startup_timeout.unwrap_or(DEFAULT_STARTUP_TIMEOUT)
    }
}

/// Configuration of one MCP server: where it is, and what it is allowed to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub transport: McpTransport,
    pub policy: McpServerPolicy,
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
                env_vars: Vec::new(),
                cwd: None,
            },
            policy: McpServerPolicy::default(),
            source: McpConfigSource::default(),
            shadows_lower_priority: false,
        }
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        self.transport.env()
    }
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
                let narrowed = existing.policy.tools.narrowed_by(&cfg.policy.tools);
                if narrowed != existing.policy.tools {
                    self.issues.push(McpConfigIssue {
                        server: name.clone(),
                        source: existing.source.clone(),
                        kind: McpConfigIssueKind::FilterNarrowed {
                            kept_source: cfg.source.clone(),
                        },
                    });
                    existing.policy.tools = narrowed;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        restrictive.policy.tools.enabled = Some(BTreeSet::from(["read".into(), "write".into()]));
        let mut high = BTreeMap::new();
        high.insert("a".to_string(), restrictive);
        let high = McpConfigFile {
            servers: high,
            skipped: 0,
            issues: Vec::new(),
        };

        let mut narrow = server("low", McpConfigOrigin::ClaudeUser);
        narrow.policy.tools.enabled = Some(BTreeSet::from(["read".into()]));
        narrow.policy.tools.disabled = BTreeSet::from(["danger".into()]);
        let mut low = BTreeMap::new();
        low.insert("a".to_string(), narrow);
        let low = McpConfigFile {
            servers: low,
            skipped: 0,
            issues: Vec::new(),
        };

        let merged = high.merge_under(low);
        let policy = &merged.servers["a"].policy.tools;
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
}
