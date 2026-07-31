//! Turning a `.mcp.json` entry into a usable server config, or into the reason
//! it is not one.
//!
//! This is where the two security rules of the format are enforced, because it
//! is the only place a workspace-controlled file is read:
//!
//! 1. **A workspace entry never widens.** An approval level or a parallelism
//!    declaration coming from a file the workspace controls is dropped with a
//!    diagnostic, exactly like the security keys of `settings.rs`. Opening a
//!    repository must not be enough to auto-approve a remote call.
//! 2. **A remote endpoint must be encrypted**, loopback aside.
//!
//! Unknown keys are ignored on purpose: `~/.claude.json` carries entries written
//! for other clients, and a key Pyxis does not know must not disable a server
//! that works everywhere else.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use super::{
    MAX_STARTUP_TIMEOUT, MAX_TOOL_TIMEOUT, McpApproval, McpConfigIssue, McpConfigIssueKind,
    McpConfigSource, McpOAuthEntry, McpServerConfig, McpServerPolicy, McpToolPolicy, McpTransport,
};

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
    #[serde(rename = "envVars", default)]
    env_vars: Vec<String>,
    cwd: Option<String>,
    url: Option<String>,
    #[serde(rename = "bearerTokenEnvVar")]
    bearer_token_env_var: Option<String>,
    #[serde(rename = "httpHeaders", default)]
    http_headers: BTreeMap<String, String>,
    #[serde(rename = "envHttpHeaders", default)]
    env_http_headers: BTreeMap<String, String>,
    #[serde(rename = "oauthClientId")]
    oauth_client_id: Option<String>,
    #[serde(rename = "oauthScopes", default)]
    oauth_scopes: Vec<String>,
    #[serde(rename = "oauthResource")]
    oauth_resource: Option<String>,
    #[serde(rename = "startupTimeoutMs")]
    startup_timeout_ms: Option<u64>,
    #[serde(rename = "toolTimeoutMs")]
    tool_timeout_ms: Option<u64>,
    #[serde(default)]
    required: bool,
    #[serde(rename = "supportsParallelToolCalls", default)]
    supports_parallel_tool_calls: bool,
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
pub(super) struct RawConfigFile {
    #[serde(default, rename = "mcpServers")]
    pub(super) mcp_servers: BTreeMap<String, serde_json::Value>,
}

/// Parses one entry. `Ok` carries the diagnostics of an entry that was accepted
/// but altered (a dropped approval); `Err` carries the single reason it was refused.
pub(super) fn parse_server(
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
    let workspace_controlled = source.origin.is_workspace_controlled();
    // FR-18: a file the workspace controls may restrict, never widen.
    if workspace_controlled && tools.widens() {
        tools = tools.without_widening();
        warnings.push(issue(name, source, McpConfigIssueKind::ApprovalIgnored));
    }
    let supports_parallel_tool_calls = if workspace_controlled && raw.supports_parallel_tool_calls {
        warnings.push(issue(name, source, McpConfigIssueKind::ParallelIgnored));
        false
    } else {
        raw.supports_parallel_tool_calls
    };
    let startup_timeout = clamp_timeout(
        raw.startup_timeout_ms,
        MAX_STARTUP_TIMEOUT,
        "startupTimeoutMs",
        name,
        source,
        &mut warnings,
    );
    let tool_timeout = clamp_timeout(
        raw.tool_timeout_ms,
        MAX_TOOL_TIMEOUT,
        "toolTimeoutMs",
        name,
        source,
        &mut warnings,
    );
    Ok((
        McpServerConfig {
            transport,
            policy: McpServerPolicy {
                tools,
                startup_timeout,
                tool_timeout,
                required: raw.required,
                supports_parallel_tool_calls,
            },
            source: source.clone(),
            shadows_lower_priority: false,
        },
        warnings,
    ))
}

/// Reads a bound in milliseconds, clamped to the ceiling the harness accepts. A
/// zero is treated as "not declared": it would otherwise mean an instant timeout.
fn clamp_timeout(
    raw_ms: Option<u64>,
    ceiling: Duration,
    key: &'static str,
    name: &str,
    source: &McpConfigSource,
    warnings: &mut Vec<McpConfigIssue>,
) -> Option<Duration> {
    let declared = Duration::from_millis(raw_ms?);
    if declared.is_zero() {
        return None;
    }
    if declared > ceiling {
        warnings.push(issue(
            name,
            source,
            McpConfigIssueKind::TimeoutClamped { key },
        ));
        return Some(ceiling);
    }
    Some(declared)
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
            http_headers: non_empty_pairs(&raw.http_headers),
            env_http_headers: non_empty_pairs(&raw.env_http_headers),
            oauth: McpOAuthEntry {
                client_id: raw
                    .oauth_client_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string),
                scopes: raw
                    .oauth_scopes
                    .iter()
                    .map(|scope| scope.trim())
                    .filter(|scope| !scope.is_empty())
                    .map(str::to_string)
                    .collect(),
                resource: raw
                    .oauth_resource
                    .as_deref()
                    .map(str::trim)
                    .filter(|resource| !resource.is_empty())
                    .map(str::to_string),
            },
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
        env_vars: raw
            .env_vars
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect(),
        cwd: raw
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(PathBuf::from),
    })
}

/// Drops the entries whose key or value is blank: a header with no name is a
/// typo, and letting it through would produce an opaque transport error later.
fn non_empty_pairs(raw: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    raw.iter()
        .filter(|(key, value)| !key.trim().is_empty() && !value.trim().is_empty())
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
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
    use std::path::Path;

    use super::super::{DEFAULT_STARTUP_TIMEOUT, McpConfigOrigin};
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
                http_headers: BTreeMap::new(),
                env_http_headers: BTreeMap::new(),
                oauth: Default::default(),
            }
        );
        // US-013 AC3: nothing readable is stored, so nothing can leak through Debug.
        assert!(!format!("{cfg:?}").contains("secret"));
    }

    #[test]
    fn http_headers_keep_literals_and_env_names_apart() {
        let cfg = parse(
            "srv",
            serde_json::json!({
                "type": "http",
                "url": "https://mcp.example.com/mcp",
                "httpHeaders": {"X-Api-Version": "2", "  ": "blank", "X-Empty": ""},
                "envHttpHeaders": {"X-Api-Key": "EXAMPLE_KEY_VAR"}
            }),
            McpConfigOrigin::ClaudeUser,
        );
        let McpTransport::Http {
            http_headers,
            env_http_headers,
            ..
        } = &cfg.transport
        else {
            unreachable!("http entry")
        };
        // Blank names and blank values are dropped rather than sent as-is.
        assert_eq!(
            http_headers,
            &BTreeMap::from([("X-Api-Version".to_string(), "2".to_string())])
        );
        // Only the NAME of the variable is stored.
        assert_eq!(
            env_http_headers,
            &BTreeMap::from([("X-Api-Key".to_string(), "EXAMPLE_KEY_VAR".to_string())])
        );
        assert_eq!(
            cfg.transport.credential_env_names(),
            vec!["EXAMPLE_KEY_VAR"]
        );
    }

    #[test]
    fn oauth_parameters_are_read_and_trimmed() {
        let cfg = parse(
            "srv",
            serde_json::json!({
                "type": "http",
                "url": "https://mcp.example.com/mcp",
                "oauthClientId": "  client-123  ",
                "oauthScopes": ["mcp.read", "  ", "mcp.write"],
                "oauthResource": "https://mcp.example.com/mcp"
            }),
            McpConfigOrigin::ClaudeUser,
        );
        let McpTransport::Http { oauth, .. } = &cfg.transport else {
            unreachable!("http entry")
        };
        assert_eq!(oauth.client_id.as_deref(), Some("client-123"));
        assert_eq!(oauth.scopes, vec!["mcp.read", "mcp.write"]);
        assert_eq!(
            oauth.resource.as_deref(),
            Some("https://mcp.example.com/mcp")
        );
        // Nothing declared: dynamic registration and the advertised scopes.
        let bare = parse(
            "srv",
            serde_json::json!({"type": "http", "url": "https://mcp.example.com/mcp"}),
            McpConfigOrigin::ClaudeUser,
        );
        let McpTransport::Http { oauth, .. } = &bare.transport else {
            unreachable!("http entry")
        };
        assert_eq!(oauth, &McpOAuthEntry::default());
    }

    #[test]
    fn stdio_forwards_named_variables_without_storing_values() {
        let cfg = parse(
            "srv",
            serde_json::json!({
                "command": "node",
                "envVars": ["GITHUB_TOKEN", "  ", ""],
                "cwd": "/srv/work"
            }),
            McpConfigOrigin::ClaudeUser,
        );
        let McpTransport::Stdio { env_vars, cwd, .. } = &cfg.transport else {
            unreachable!("stdio entry")
        };
        assert_eq!(env_vars, &vec!["GITHUB_TOKEN".to_string()]);
        assert_eq!(cwd.as_deref(), Some(Path::new("/srv/work")));
        assert_eq!(cfg.transport.credential_env_names(), vec!["GITHUB_TOKEN"]);
    }

    #[test]
    fn timeouts_are_read_and_clamped() {
        let source = McpConfigSource::new(McpConfigOrigin::ClaudeUser, "");
        let (cfg, warnings) = parse_server(
            "srv",
            serde_json::json!({
                "command": "x",
                "startupTimeoutMs": 30_000,
                "toolTimeoutMs": 9_000_000
            }),
            &source,
        )
        .expect("entry stays usable");
        assert_eq!(cfg.policy.startup_timeout, Some(Duration::from_secs(30)));
        assert_eq!(cfg.policy.startup_timeout(), Duration::from_secs(30));
        assert_eq!(cfg.policy.tool_timeout, Some(MAX_TOOL_TIMEOUT));
        assert_eq!(
            warnings[0].kind,
            McpConfigIssueKind::TimeoutClamped {
                key: "toolTimeoutMs"
            }
        );
        // Undeclared: the harness default applies.
        let bare = parse(
            "srv",
            serde_json::json!({"command": "x"}),
            McpConfigOrigin::ClaudeUser,
        );
        assert_eq!(bare.policy.startup_timeout, None);
        assert_eq!(bare.policy.startup_timeout(), DEFAULT_STARTUP_TIMEOUT);
        // Zero would mean an instant timeout: treated as undeclared.
        let zero = parse(
            "srv",
            serde_json::json!({"command": "x", "startupTimeoutMs": 0}),
            McpConfigOrigin::ClaudeUser,
        );
        assert_eq!(zero.policy.startup_timeout, None);
    }

    #[test]
    fn a_workspace_file_cannot_lift_the_serialization() {
        let source = McpConfigSource::new(McpConfigOrigin::Workspace, "");
        let (cfg, warnings) = parse_server(
            "srv",
            serde_json::json!({"command": "x", "supportsParallelToolCalls": true}),
            &source,
        )
        .expect("entry stays usable");
        assert!(!cfg.policy.supports_parallel_tool_calls);
        assert_eq!(warnings[0].kind, McpConfigIssueKind::ParallelIgnored);
        // A user file may declare it.
        let user = parse(
            "srv",
            serde_json::json!({"command": "x", "supportsParallelToolCalls": true}),
            McpConfigOrigin::ClaudeUser,
        );
        assert!(user.policy.supports_parallel_tool_calls);
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
        assert!(cfg.policy.tools.exposes("read"));
        // Listed in both: the deny-list applies last, hence wins.
        assert!(!cfg.policy.tools.exposes("write"));
        assert!(!cfg.policy.tools.exposes("delete"));
        assert!(!cfg.policy.tools.exposes("other"));
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
        assert_eq!(cfg.policy.tools.approval_for("read"), McpApproval::Allow);
        assert_eq!(cfg.policy.tools.approval_for("delete"), McpApproval::Ask);
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
        assert_eq!(cfg.policy.tools.approval_for("read"), McpApproval::Ask);
        assert_eq!(cfg.policy.tools.approval_for("other"), McpApproval::Ask);
        // The restriction survives: only the widening is dropped.
        assert!(!cfg.policy.tools.exposes("delete"));
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
            cfg.policy.tools.unknown_names(&available),
            vec!["ghost", "phantom"]
        );
    }
}
