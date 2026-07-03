//! Client MCP : connexion à un serveur via transport stdio (`rmcp`), handshake
//! `initialize` automatique, liste des outils. Le wrapping des outils en `DynTool`
//! (intégration au registre `agent-tools`) viendra en Phase 2.

use std::sync::Arc;
use std::time::Duration;

use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use tokio::process::Command;

use crate::config::McpServerConfig;
use crate::error::McpError;

pub type CommandHardener = Arc<dyn Fn(&mut Command) + Send + Sync>;

/// Délai max d'établissement de la connexion (spawn + handshake `initialize`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(10);

/// Plafond de longueur d'une description d'outil (ARCHITECTURE §6 : un serveur ne
/// peut pas polluer le prompt).
const DESCRIPTION_CAP: usize = 2048;

/// Connexion vivante à un serveur MCP stdio. Détient le `RunningService` : sa
/// fermeture (`cancel`) ou son drop tue le sous-process.
pub struct McpConnection {
    service: RunningService<RoleClient, ()>,
}

/// Métadonnée d'un outil exposé. Les schémas restent attachés ici pour permettre
/// une future exposition modèle via adapter strict, sans refaire un handshake.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub original_name: String,
    pub title: Option<String>,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: Option<serde_json::Value>,
    /// Les annotations MCP sont des hints fournis par le serveur distant. Elles ne
    /// doivent jamais devenir une décision de sécurité côté client.
    pub annotations_untrusted: bool,
}

impl McpConnection {
    /// Spawn le serveur stdio et établit le handshake MCP. `name` sert au libellé
    /// d'erreur.
    pub async fn connect(name: &str, cfg: &McpServerConfig) -> Result<Self, McpError> {
        Self::connect_hardened(name, cfg, None).await
    }

    /// Variante durcie : le caller peut injecter le même scrub env + proxy que les
    /// outils Bash. `cfg.env` reste explicite, mais les clés proxy sont ignorées
    /// pour éviter les bypass via `NO_PROXY` ou `ALL_PROXY`.
    pub async fn connect_hardened(
        name: &str,
        cfg: &McpServerConfig,
        harden: Option<&CommandHardener>,
    ) -> Result<Self, McpError> {
        let mut command = Command::new(&cfg.command);
        command.args(&cfg.args);
        if let Some(harden) = harden {
            harden(&mut command);
        }
        for (k, v) in &cfg.env {
            if is_proxy_env_key(k) {
                continue;
            }
            command.env(k, v);
        }
        let transport = TokioChildProcess::new(command).map_err(|e| McpError::Spawn {
            server: name.to_string(),
            source: e,
        })?;
        // Sur timeout, le futur `serve()` est droppé en place et le sous-process est
        // tué via le `Drop` du transport (kill détaché). Suffisant pour une CLI
        // longue-durée ; un arrêt gracieux explicite (serve_with_ct) reste possible.
        let service: RunningService<RoleClient, ()> =
            tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport))
                .await
                .map_err(|_| McpError::Connect {
                    server: name.to_string(),
                    message: format!("timeout après {}s", CONNECT_TIMEOUT.as_secs()),
                })?
                .map_err(|e| McpError::Connect {
                    server: name.to_string(),
                    message: e.to_string(),
                })?;
        Ok(Self { service })
    }

    /// Liste les outils exposés par le serveur (descriptions cappées à 2048 chars).
    pub async fn list_tools(&self, name: &str) -> Result<Vec<McpToolInfo>, McpError> {
        let tools = tokio::time::timeout(LIST_TOOLS_TIMEOUT, self.service.list_all_tools())
            .await
            .map_err(|_| McpError::Connect {
                server: name.to_string(),
                message: format!("list_tools timeout après {}s", LIST_TOOLS_TIMEOUT.as_secs()),
            })?
            .map_err(|e| McpError::Connect {
                server: name.to_string(),
                message: format!("list_tools : {e}"),
            })?;
        Ok(tools
            .into_iter()
            .map(|t| McpToolInfo {
                name: t.name.to_string(),
                original_name: t.name.into_owned(),
                title: t.title,
                description: t
                    .description
                    .map(|d| cap(&d, DESCRIPTION_CAP))
                    .unwrap_or_default(),
                input_schema: serde_json::Value::Object((*t.input_schema).clone()),
                output_schema: t
                    .output_schema
                    .map(|schema| serde_json::Value::Object((*schema).clone())),
                annotations_untrusted: t.annotations.is_some(),
            })
            .collect())
    }

    /// Ferme proprement la connexion (stdin fermé, attente bornée, puis kill).
    ///
    /// Le `Result` de `cancel()` (un `JoinError` si la tâche de service a paniqué)
    /// est volontairement ignoré : le sous-process est de toute façon tué par le
    /// `Drop` du transport. Appelé en fire-and-forget.
    pub async fn cancel(self) {
        let _ = self.service.cancel().await;
    }
}

/// Tronque `s` à `max` chars (jamais au milieu d'un char multi-octet).
fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

fn is_proxy_env_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "http_proxy" | "https_proxy" | "all_proxy" | "no_proxy"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_env_keys_are_filtered_case_insensitively() {
        assert!(is_proxy_env_key("HTTP_PROXY"));
        assert!(is_proxy_env_key("https_proxy"));
        assert!(is_proxy_env_key("All_Proxy"));
        assert!(is_proxy_env_key("NO_PROXY"));
        assert!(!is_proxy_env_key("PATH"));
        assert!(!is_proxy_env_key("API_TOKEN"));
    }
}
