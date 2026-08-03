//! `/mcp` and the connections it starts.
//!
//! A connection (spawn + handshake + `tools/list`) never runs inside the event
//! loop: it is spawned, and its outcome comes back as an [`McpEvent`] the loop
//! handles like any other wake-up. That is what keeps the TUI responsive while a
//! remote server is being dialed, and what makes the OAuth round trip possible
//! at all.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use agent_tui::{Block, McpServerMeta, McpStatus};
use tokio::sync::mpsc;

use super::Loop;

/// Result of an MCP connection started in the background. Comes back into the
/// `select!` loop to update the registry and the display without freezing the TUI.
pub(super) enum McpEvent {
    Connected {
        name: String,
        conn: agent_mcp::McpConnection,
        tools: Vec<agent_mcp::McpToolInfo>,
    },
    Failed {
        name: String,
        error: String,
    },
    /// The authorization URL of a login in flight. Emitted as soon as it is
    /// known, not at the end: a browser that failed to open must not leave the
    /// user waiting on a URL they never see (FR-15).
    LoginUrl {
        name: String,
        url: String,
        opened: bool,
    },
    /// A per-server OAuth login completed: the credential is in the keyring and
    /// the next connection will pick it up.
    LoggedIn {
        name: String,
    },
    LoggedOut {
        name: String,
    },
    LoginFailed {
        name: String,
        error: String,
    },
    /// Result of `/mcp <server> resources`, already rendered.
    Resources {
        name: String,
        summary: String,
    },
}

impl Loop {
    /// Handles `/mcp [<server> <action>]`.
    pub(super) fn mcp_command(&mut self, arg: &str) {
        if arg == "issues" {
            self.show_mcp_issues();
            return;
        }
        let Some((server, action)) = arg.rsplit_once(' ') else {
            self.state.blocks.push(Block::Notice(
                "Select a server and then an action in the /mcp submenu. Diagnostics: /mcp issues."
                    .into(),
            ));
            return;
        };
        let server = server.trim().to_string();
        if server.is_empty() {
            self.state
                .blocks
                .push(Block::Notice("Usage: /mcp <server> <action>.".into()));
            return;
        }
        match action {
            "connect" | "reconnect" => {
                if let Some(cfg) = config_for(&self.mcp, &server)
                    && mcp_requires_trust(&cfg)
                {
                    let lead = format!(
                        "Connection blocked before spawn. Retry with /mcp {server} trust."
                    );
                    self.state
                        .blocks
                        .push(Block::Notice(trust_notice(&server, &cfg, &lead)));
                    return;
                }
                self.start_connect(&server, None);
            }
            "trust" => {
                let Some(cfg) = config_for(&self.mcp, &server) else {
                    self.state
                        .blocks
                        .push(Block::Notice(format!("Unknown MCP server: {server}.")));
                    return;
                };
                self.state.blocks.push(Block::Notice(trust_notice(
                    &server,
                    &cfg,
                    "Trust confirmed for this connection.",
                )));
                self.start_connect(&server, Some(cfg));
            }
            "disconnect" => self.disconnect(&server),
            "tools" => self.show_tools(&server),
            // `initialize.instructions` is inspected here and reaches the model
            // nowhere. A tool description travels inside the tool definitions, which
            // no tool output ever taints, so server prose folded in there would be
            // injection the taint defense is structurally unable to see.
            "info" => {
                let notice = match self.mcp.lock().ok().and_then(|reg| {
                    reg.get(&server)
                        .map(|srv| srv.instructions().map(str::to_string))
                }) {
                    Some(Some(text)) => {
                        format!("MCP \"{server}\" states (untrusted, server-authored):\n{text}")
                    }
                    Some(None) => {
                        format!("MCP \"{server}\": the server declared no instructions.")
                    }
                    None => format!("Unknown MCP server: {server}."),
                };
                self.state.blocks.push(Block::Notice(notice));
            }
            "resources" => self.list_resources(&server),
            // Per-server OAuth (MCP authorization). The browser round trip runs in
            // the background: the TUI owns the terminal and must not block on it.
            // Its result comes back through the same channel as a connection.
            "login" => self.start_login(&server),
            "logout" => {
                let tx = self.mcp_tx.clone();
                let name = server;
                tokio::spawn(async move {
                    let outcome = tokio::task::spawn_blocking({
                        let name = name.clone();
                        move || agent_auth::oauth::mcp::delete(&name)
                    })
                    .await;
                    let ev = match outcome {
                        Ok(Ok(())) => McpEvent::LoggedOut { name },
                        Ok(Err(err)) => McpEvent::LoginFailed {
                            name,
                            error: format!("logout failed: {err}"),
                        },
                        Err(err) => McpEvent::LoginFailed {
                            name,
                            error: format!("logout task: {err}"),
                        },
                    };
                    let _ = tx.send(ev).await;
                });
            }
            other => self
                .state
                .blocks
                .push(Block::Notice(format!("Unknown MCP action: {other}"))),
        }
    }

    fn start_connect(&mut self, server: &str, trusted_cfg: Option<agent_mcp::McpServerConfig>) {
        let begin = match self.mcp.lock() {
            Ok(mut reg) => reg.begin_connect(server),
            Err(_) => Err(agent_mcp::McpError::Unknown(server.to_string())),
        };
        let (cfg_srv, old) = match begin {
            Ok(pair) => pair,
            Err(err) => {
                self.state
                    .blocks
                    .push(Block::Notice(format!("MCP: {err}")));
                return;
            }
        };
        // The whole config is compared, not just the command: a transport, an
        // endpoint or a tool policy swapped between the prompt and the spawn
        // would make the confirmation meaningless.
        if let Some(expected) = trusted_cfg
            && expected != cfg_srv
        {
            if let Some(old) = old {
                tokio::spawn(async move { old.cancel().await });
            }
            if let Ok(mut reg) = self.mcp.lock() {
                reg.fail(server, "MCP config changed during trust".to_string());
            }
            self.state.blocks.push(Block::Error(format!(
                "MCP \"{server}\": config changed during trust."
            )));
            self.state.mcp_servers = metas(&self.mcp);
            return;
        }
        if let Some(old) = old {
            tokio::spawn(async move { old.cancel().await });
        }
        self.state.mcp_servers = metas(&self.mcp);
        self.state
            .blocks
            .push(Block::Notice(format!("MCP \"{server}\": connecting...")));
        let tx = self.mcp_tx.clone();
        let name = server.to_string();
        let harden = Arc::clone(&self.cfg.command_hardener);
        tokio::spawn(async move {
            let token = crate::mcp_oauth_token(&cfg_srv, &name).await;
            let ev = match agent_mcp::McpConnection::connect_with(
                &name,
                &cfg_srv,
                Some(&harden),
                token.as_deref(),
            )
            .await
            {
                Ok(conn) => match conn.list_tools(&name).await {
                    Ok(tools) => McpEvent::Connected { name, conn, tools },
                    Err(err) => {
                        conn.cancel().await;
                        McpEvent::Failed {
                            name,
                            error: err.to_string(),
                        }
                    }
                },
                Err(err) => McpEvent::Failed {
                    name,
                    error: err.to_string(),
                },
            };
            // Closed channel: recover the connection and close the subprocess.
            if let Err(mpsc::error::SendError(ev)) = tx.send(ev).await
                && let McpEvent::Connected { conn, .. } = ev
            {
                conn.cancel().await;
            }
        });
    }

    fn disconnect(&mut self, server: &str) {
        let old = self
            .mcp
            .lock()
            .ok()
            .and_then(|mut reg| reg.begin_disconnect(server));
        match old {
            Some(old) => {
                tokio::spawn(async move { old.cancel().await });
                // US-016 AC2: its tools leave the registry at the next turn
                // boundary; a later call then fails as an unknown tool rather
                // than reaching a dead connection.
                let removed = self.mcp_tool_names.remove(server).unwrap_or_default();
                let count = removed.len();
                self.cfg
                    .registry
                    .stage_removal(removed.into_iter().collect());
                // Withdrawn immediately, unlike the tools: a resource read has
                // no staged view to protect, and reaching a cancelled peer would
                // fail with a transport error instead of the "not connected" the
                // model can act on.
                self.cfg.mcp_resources.disconnect(server);
                self.state.blocks.push(Block::Notice(format!(
                    "MCP \"{server}\" disconnected ({count} tools withdrawn from \
                     the next turn on)."
                )));
            }
            None => self
                .state
                .blocks
                .push(Block::Notice(format!("MCP \"{server}\" is not connected."))),
        }
        self.state.mcp_servers = metas(&self.mcp);
    }

    fn show_tools(&mut self, server: &str) {
        let Ok(reg) = self.mcp.lock() else {
            return;
        };
        let notice = match reg.get(server) {
            Some(srv) if !srv.tools().is_empty() => {
                let names = srv
                    .tools()
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("MCP \"{server}\" ({} tools): {names}", srv.tools().len())
            }
            Some(_) => format!("MCP \"{server}\": no exposed tools."),
            None => format!("Unknown MCP server: {server}."),
        };
        drop(reg);
        self.state.blocks.push(Block::Notice(notice));
    }

    /// Resources are inspected, never handed to the model: a server does not get
    /// to push content into a turn nobody asked for.
    fn list_resources(&mut self, server: &str) {
        // The call handle is cloned out of the registry, so the lock is released
        // before the listing is awaited.
        let client = {
            let Ok(reg) = self.mcp.lock() else { return };
            match reg.get(server) {
                Some(agent_mcp::McpServer::Connected { conn, .. }) => conn.client(server),
                _ => {
                    drop(reg);
                    self.state
                        .blocks
                        .push(Block::Notice(format!("MCP \"{server}\" is not connected.")));
                    return;
                }
            }
        };
        let tx = self.mcp_tx.clone();
        let name = server.to_string();
        tokio::spawn(async move {
            let ev = match client.list_resources().await {
                Ok(resources) if resources.is_empty() => McpEvent::Resources {
                    name,
                    summary: "no resources advertised".to_string(),
                },
                Ok(resources) => {
                    let shown = resources
                        .iter()
                        .take(20)
                        .map(|resource| match &resource.description {
                            Some(description) => format!("  {} - {}", resource.uri, description),
                            None => format!("  {}", resource.uri),
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let more = resources.len().saturating_sub(20);
                    let summary = if more > 0 {
                        format!("{} resources:\n{shown}\n  ... {more} more", resources.len())
                    } else {
                        format!("{} resources:\n{shown}", resources.len())
                    };
                    McpEvent::Resources { name, summary }
                }
                Err(err) => McpEvent::Resources {
                    name,
                    summary: err.to_string(),
                },
            };
            let _ = tx.send(ev).await;
        });
    }

    fn start_login(&mut self, server: &str) {
        let Some(cfg) = config_for(&self.mcp, server) else {
            self.state
                .blocks
                .push(Block::Notice(format!("Unknown MCP server: {server}.")));
            return;
        };
        let agent_mcp::McpTransport::Http { url, oauth, .. } = &cfg.transport else {
            self.state.blocks.push(Block::Notice(format!(
                "MCP \"{server}\" is a stdio server: it has no OAuth endpoint to log in to."
            )));
            return;
        };
        let request = agent_auth::oauth::mcp::McpOAuthRequest {
            server: server.to_string(),
            url: url.clone(),
            client_id: oauth.client_id.clone(),
            scopes: oauth.scopes.clone(),
            resource: oauth.resource.clone(),
        };
        self.state.blocks.push(Block::Notice(format!(
            "MCP \"{server}\": opening the browser to authorize..."
        )));
        let tx = self.mcp_tx.clone();
        let name = server.to_string();
        tokio::spawn(async move {
            let ev = run_login(&name, request, &tx).await;
            let _ = tx.send(ev).await;
        });
    }

    fn show_mcp_issues(&mut self) {
        let Ok(reg) = self.mcp.lock() else {
            self.state
                .blocks
                .push(Block::Error("MCP: registry unavailable.".into()));
            return;
        };
        if reg.issues().is_empty() {
            drop(reg);
            self.state
                .blocks
                .push(Block::Notice("MCP: no config diagnostics.".into()));
            return;
        }
        let mut lines = reg
            .issues()
            .iter()
            .take(12)
            .map(agent_mcp::McpConfigIssue::summary)
            .collect::<Vec<_>>();
        if reg.issue_count() > lines.len() {
            lines.push(format!(
                "{} more diagnostics",
                reg.issue_count() - lines.len()
            ));
        }
        drop(reg);
        self.state.blocks.push(Block::Notice(format!(
            "Diagnostics MCP:\n{}",
            lines.join("\n")
        )));
    }

    /// Applies the outcome of a background connection, login or listing.
    pub(super) fn on_mcp(&mut self, event: Option<McpEvent>) {
        let Some(event) = event else {
            return;
        };
        match event {
            McpEvent::Connected { name, conn, tools } => self.on_connected(name, conn, tools),
            McpEvent::Failed { name, error } => {
                if let Ok(mut reg) = self.mcp.lock() {
                    reg.fail(&name, error.clone());
                }
                self.state
                    .blocks
                    .push(Block::Error(format!("MCP \"{name}\": {error}")));
            }
            McpEvent::LoginUrl { name, url, opened } => {
                let lead = if opened {
                    "browser opened. If nothing appears, open this URL"
                } else {
                    "open this URL to authorize"
                };
                self.state
                    .blocks
                    .push(Block::Notice(format!("MCP \"{name}\": {lead}:\n{url}")));
            }
            McpEvent::LoggedIn { name } => self.state.blocks.push(Block::Notice(format!(
                "MCP \"{name}\": authorized. Run /mcp {name} connect to use it."
            ))),
            McpEvent::LoggedOut { name } => self.state.blocks.push(Block::Notice(format!(
                "MCP \"{name}\": stored authorization forgotten (reconnect to apply)."
            ))),
            McpEvent::LoginFailed { name, error } => self
                .state
                .blocks
                .push(Block::Error(format!("MCP \"{name}\" login: {error}"))),
            McpEvent::Resources { name, summary } => self
                .state
                .blocks
                .push(Block::Notice(format!("MCP \"{name}\": {summary}"))),
        }
        self.state.mcp_servers = metas(&self.mcp);
    }

    fn on_connected(
        &mut self,
        name: String,
        conn: agent_mcp::McpConnection,
        tools: Vec<agent_mcp::McpToolInfo>,
    ) {
        // US-014: the entry of THIS server shapes what is exposed, before
        // anything reaches the tool registry. No entry left (the server was
        // removed mid-connect): the default policy is the fail-closed one, so an
        // orphan connection exposes nothing auto-approved.
        let policy = config_for(&self.mcp, &name)
            .map(|cfg| cfg.policy)
            .unwrap_or_default();
        let (tools, filter_notices) = agent_mcp::filter_tools(&name, &tools, &policy.tools);
        for notice in filter_notices {
            self.state.blocks.push(Block::Notice(notice));
        }
        // Reconnect: the names this server held are released (and staged out)
        // before new ones are handed out, so a name is never handed to two tools.
        if let Some(previous) = self.mcp_tool_names.remove(&name) {
            self.cfg
                .registry
                .stage_removal(previous.into_iter().collect());
        }
        let mut taken: BTreeSet<String> = self.mcp_tool_names.values().flatten().cloned().collect();
        let client = conn.client(&name);
        let (exposed, skipped) =
            agent_mcp::dyn_tools(&name, &tools, &policy, &client, &mut taken);
        for skip in skipped {
            self.state.blocks.push(Block::Notice(skip.summary()));
        }
        let count = exposed.len();
        let names: BTreeSet<String> = exposed.iter().map(|tool| tool.name().to_string()).collect();
        // Poisoned lock: close the connection instead of silently dropping it.
        let held = match self.mcp.lock() {
            Ok(mut reg) => match reg.finish_connect(&name, conn, tools) {
                // Disconnected while connecting: cancel the orphan session.
                Some(orphan) => {
                    tokio::spawn(async move { orphan.cancel().await });
                    self.state.blocks.push(Block::Notice(format!(
                        "MCP \"{name}\": connection canceled."
                    )));
                    false
                }
                None => true,
            },
            Err(_) => {
                tokio::spawn(async move { conn.cancel().await });
                self.state.blocks.push(Block::Error(
                    "MCP: registry unavailable, connection closed.".into(),
                ));
                false
            }
        };
        if !held {
            return;
        }
        // US-016: staged, not registered. The exposed set only moves at a turn
        // boundary, so a turn in flight keeps the tools it was given.
        self.cfg.registry.stage_tools(exposed);
        // US-012: the resource tools follow the held connection, not the staged
        // tools: they were registered at startup and read the catalog at call
        // time.
        self.cfg.mcp_resources.connect(name.clone(), client);
        self.mcp_tool_names.insert(name.clone(), names);
        self.state.blocks.push(Block::Notice(format!(
            "MCP \"{name}\" connected ({count} tools), callable from the next turn."
        )));
    }
}

/// Runs one OAuth login to completion and reports it as an `McpEvent`.
///
/// The authorization URL is printed as a notice rather than by the auth crate:
/// only the binary knows a TUI owns the terminal (FR-15, US-020).
async fn run_login(
    server: &str,
    request: agent_auth::oauth::mcp::McpOAuthRequest,
    tx: &mpsc::Sender<McpEvent>,
) -> McpEvent {
    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            return McpEvent::LoginFailed {
                name: server.to_string(),
                error: format!("http client: {err}"),
            };
        }
    };
    let announce = tx.clone();
    let name = server.to_string();
    let outcome = agent_auth::oauth::mcp::login(
        &client,
        &request,
        agent_auth::oauth::now_ms(),
        move |url, opened| {
            // `try_send` rather than an await: the callback is synchronous, and a
            // full channel is not a reason to abort a login already in flight.
            let _ = announce.try_send(McpEvent::LoginUrl {
                name,
                url: url.to_string(),
                opened,
            });
        },
    )
    .await;
    match outcome {
        Ok(cred) => {
            // The keyring is blocking: it must not run on the async runtime.
            let stored =
                tokio::task::spawn_blocking(move || agent_auth::oauth::mcp::save(&cred)).await;
            match stored {
                Ok(Ok(())) => McpEvent::LoggedIn {
                    name: server.to_string(),
                },
                Ok(Err(err)) => McpEvent::LoginFailed {
                    name: server.to_string(),
                    error: format!("keyring: {err}"),
                },
                Err(err) => McpEvent::LoginFailed {
                    name: server.to_string(),
                    error: format!("keyring task: {err}"),
                },
            }
        }
        Err(err) => McpEvent::LoginFailed {
            name: server.to_string(),
            error: err.to_string(),
        },
    }
}

fn config_for(
    mcp: &Arc<Mutex<agent_mcp::McpRegistry>>,
    server: &str,
) -> Option<agent_mcp::McpServerConfig> {
    mcp.lock()
        .ok()
        .and_then(|reg| reg.get(server).map(|srv| srv.config().clone()))
}

/// Does this server need an explicit `/mcp <server> trust` before being spawned?
/// A workspace-controlled declaration, a config shadowing a user entry, and a
/// sensitive env key are the three cases where a repository could otherwise obtain
/// an execution. The startup connection (`main`) honors the same gate.
pub(crate) fn mcp_requires_trust(cfg: &agent_mcp::McpServerConfig) -> bool {
    matches!(cfg.source.origin, agent_mcp::McpConfigOrigin::Workspace)
        || cfg.shadows_lower_priority
        || !sensitive_env_keys(cfg).is_empty()
}

fn sensitive_env_keys(cfg: &agent_mcp::McpServerConfig) -> Vec<&str> {
    cfg.env()
        .keys()
        .map(String::as_str)
        .filter(|key| {
            let upper = key.to_ascii_uppercase();
            matches!(
                upper.as_str(),
                "PATH"
                    | "LD_PRELOAD"
                    | "LD_LIBRARY_PATH"
                    | "DYLD_INSERT_LIBRARIES"
                    | "DYLD_LIBRARY_PATH"
                    | "NODE_OPTIONS"
                    | "PYTHONPATH"
                    | "RUBYOPT"
                    | "BUNDLE_GEMFILE"
                    | "CARGO_HOME"
                    | "RUSTUP_HOME"
            )
        })
        .collect()
}

fn trust_notice(server: &str, cfg: &agent_mcp::McpServerConfig, lead: &str) -> String {
    // The NAMES of the variables a connection would read are shown: they say
    // which credentials of the machine would reach that server. No value is read.
    let credentials = match cfg.transport.credential_env_names() {
        names if names.is_empty() => "(none)".to_string(),
        names => names.join(", "),
    };
    let detail = match &cfg.transport {
        agent_mcp::McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
            ..
        } => {
            let args = if args.is_empty() {
                "(none)".to_string()
            } else {
                args.join(" ")
            };
            let env_keys = if env.is_empty() {
                "(none)".to_string()
            } else {
                env.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            let sensitive = sensitive_env_keys(cfg);
            let sensitive = if sensitive.is_empty() {
                "(none)".to_string()
            } else {
                sensitive.join(", ")
            };
            let cwd = match cwd {
                Some(cwd) => format!("\nWorking directory: {}", cwd.display()),
                None => String::new(),
            };
            format!(
                "Command: {command}\nArgs: {args}\nEnv: {env_keys} (values masked)\nSensitive env: {sensitive}\nForwarded from your environment: {credentials}{cwd}"
            )
        }
        agent_mcp::McpTransport::Http {
            url, http_headers, ..
        } => {
            let headers = if http_headers.is_empty() {
                "(none)".to_string()
            } else {
                http_headers.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            format!(
                "Endpoint: {url}\nCredentials read from your environment: {credentials}\nExtra headers: {headers}"
            )
        }
    };
    let shadow = if cfg.shadows_lower_priority {
        "\nShadowing: hides a lower-priority MCP config."
    } else {
        ""
    };
    format!(
        "MCP \"{server}\": {lead}\nSource: {}\nTransport: {}\n{detail}{shadow}",
        cfg.source.display(),
        cfg.transport.short_label(),
    )
}

/// Projects the MCP registry into display metadata for the `/mcp` submenu.
pub(super) fn metas(mcp: &Arc<Mutex<agent_mcp::McpRegistry>>) -> Vec<McpServerMeta> {
    let Ok(reg) = mcp.lock() else {
        return Vec::new();
    };
    reg.iter()
        .map(|(name, server)| McpServerMeta {
            name: name.clone(),
            status: match server {
                agent_mcp::McpServer::Disconnected { .. } => McpStatus::Disconnected,
                agent_mcp::McpServer::Connecting { .. } => McpStatus::Connecting,
                agent_mcp::McpServer::Connected { .. } => McpStatus::Connected,
                agent_mcp::McpServer::Failed { .. } => McpStatus::Failed,
            },
            source: server.config().source.short_label().to_string(),
            needs_trust: mcp_requires_trust(server.config()),
            tool_count: server.tool_count(),
            remote: matches!(
                server.config().transport,
                agent_mcp::McpTransport::Http { .. }
            ),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// US-013 AC5: the startup connection (US-012) and `/mcp connect` share this
    /// gate, so a repository-controlled declaration never earns a spawn on its own.
    #[test]
    fn a_workspace_controlled_server_stays_behind_the_trust_gate() {
        use agent_mcp::{
            McpConfigOrigin, McpConfigSource, McpServerConfig, McpServerPolicy, McpTransport,
        };
        use std::collections::BTreeMap;

        let server = |origin: McpConfigOrigin, shadows: bool, env: BTreeMap<String, String>| {
            McpServerConfig {
                transport: McpTransport::Stdio {
                    command: "srv".into(),
                    args: Vec::new(),
                    env,
                    env_vars: Vec::new(),
                    cwd: None,
                },
                policy: McpServerPolicy::default(),
                source: McpConfigSource::new(origin, ""),
                shadows_lower_priority: shadows,
            }
        };

        // User-scope, no shadowing, no sensitive env: connected at startup.
        assert!(!mcp_requires_trust(&server(
            McpConfigOrigin::ClaudeUser,
            false,
            BTreeMap::new()
        )));
        // Declared by the workspace: never auto-connected.
        assert!(mcp_requires_trust(&server(
            McpConfigOrigin::Workspace,
            false,
            BTreeMap::new()
        )));
        // Hides a user entry: same treatment.
        assert!(mcp_requires_trust(&server(
            McpConfigOrigin::ClaudeUser,
            true,
            BTreeMap::new()
        )));
        // Carries an env key that changes what gets executed.
        let mut env = BTreeMap::new();
        env.insert("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string());
        assert!(mcp_requires_trust(&server(
            McpConfigOrigin::ClaudeUser,
            false,
            env
        )));

        // US-013 AC4: the transport changes nothing. A remote server declared by
        // the workspace stays behind the same gate, and handing a credential to a
        // remote endpoint is exactly what must not happen on `cd` alone.
        let remote = |origin: McpConfigOrigin| McpServerConfig {
            transport: McpTransport::Http {
                url: "https://mcp.example.com/mcp".into(),
                bearer_token_env_var: Some("EXAMPLE_TOKEN".into()),
                http_headers: BTreeMap::new(),
                env_http_headers: BTreeMap::new(),
                oauth: Default::default(),
            },
            policy: McpServerPolicy::default(),
            source: McpConfigSource::new(origin, ""),
            shadows_lower_priority: false,
        };
        assert!(mcp_requires_trust(&remote(McpConfigOrigin::Workspace)));
        assert!(!mcp_requires_trust(&remote(McpConfigOrigin::ClaudeUser)));
    }
}
