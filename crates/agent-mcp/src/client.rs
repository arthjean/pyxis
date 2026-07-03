//! Client MCP : connexion à un serveur via transport stdio (`rmcp`), handshake
//! `initialize` automatique, liste des outils. Le wrapping des outils en `DynTool`
//! (intégration au registre `agent-tools`) viendra en Phase 2.

use std::time::Duration;

use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use tokio::process::Command;

use crate::config::McpServerConfig;
use crate::error::McpError;

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

/// Métadonnée légère d'un outil exposé (nom + description cappée). Représentation
/// d'affichage ; le `DynTool` complet arrive avec l'intégration au registre.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

impl McpConnection {
    /// Spawn le serveur stdio et établit le handshake MCP. `name` sert au libellé
    /// d'erreur. L'environnement courant est hérité (PATH, etc.) + `cfg.env`.
    pub async fn connect(name: &str, cfg: &McpServerConfig) -> Result<Self, McpError> {
        let mut command = Command::new(&cfg.command);
        command.args(&cfg.args);
        for (k, v) in &cfg.env {
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
                name: t.name.into_owned(),
                description: t
                    .description
                    .map(|d| cap(&d, DESCRIPTION_CAP))
                    .unwrap_or_default(),
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
