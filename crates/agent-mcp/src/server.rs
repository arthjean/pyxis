//! État runtime des serveurs MCP : enum discriminé (le client n'est accessible que
//! dans `Connected`, garanti à la compilation) + registre indexé par nom.

use std::collections::BTreeMap;

use crate::client::{McpConnection, McpToolInfo};
use crate::config::{McpConfigFile, McpConfigIssue, McpServerConfig};
use crate::error::McpError;

/// État d'un serveur MCP. Le `conn` n'existe que dans `Connected` : impossible
/// d'appeler un serveur non connecté.
pub enum McpServer {
    Disconnected {
        config: McpServerConfig,
    },
    Connecting {
        config: McpServerConfig,
    },
    Connected {
        config: McpServerConfig,
        conn: McpConnection,
        tools: Vec<McpToolInfo>,
    },
    Failed {
        config: McpServerConfig,
        error: String,
    },
}

impl McpServer {
    pub fn config(&self) -> &McpServerConfig {
        match self {
            McpServer::Disconnected { config }
            | McpServer::Connecting { config }
            | McpServer::Connected { config, .. }
            | McpServer::Failed { config, .. } => config,
        }
    }

    /// Nombre d'outils exposés (0 hors `Connected`).
    pub fn tool_count(&self) -> usize {
        match self {
            McpServer::Connected { tools, .. } => tools.len(),
            _ => 0,
        }
    }

    /// Outils exposés (vide hors `Connected`).
    pub fn tools(&self) -> &[McpToolInfo] {
        match self {
            McpServer::Connected { tools, .. } => tools,
            _ => &[],
        }
    }
}

/// Registre des serveurs MCP connus, indexé par nom (ordre lexicographique stable).
/// Les transitions d'état sont synchrones ; la connexion réseau elle-même se fait
/// hors du registre (l'appelant relâche le verrou avant le `await`).
#[derive(Default)]
pub struct McpRegistry {
    servers: BTreeMap<String, McpServer>,
    issues: Vec<McpConfigIssue>,
}

impl McpRegistry {
    /// Construit le registre depuis la config : tous les serveurs `Disconnected`.
    pub fn from_config(file: McpConfigFile) -> Self {
        let servers = file
            .servers
            .into_iter()
            .map(|(name, config)| (name, McpServer::Disconnected { config }))
            .collect();
        Self {
            servers,
            issues: file.issues,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&McpServer> {
        self.servers.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &McpServer)> {
        self.servers.iter()
    }

    pub fn issues(&self) -> &[McpConfigIssue] {
        &self.issues
    }

    pub fn issue_count(&self) -> usize {
        self.issues.len()
    }

    /// Passe un serveur en `Connecting` ; renvoie sa config (à spawner) et
    /// l'éventuelle connexion précédente (cas reconnect : à fermer côté appelant).
    pub fn begin_connect(
        &mut self,
        name: &str,
    ) -> Result<(McpServerConfig, Option<McpConnection>), McpError> {
        let server = self
            .servers
            .get_mut(name)
            .ok_or_else(|| McpError::Unknown(name.to_string()))?;
        // Déjà en cours de connexion → on refuse (évite un second spawn de process).
        if matches!(server, McpServer::Connecting { .. }) {
            return Err(McpError::Connect {
                server: name.to_string(),
                message: "connexion déjà en cours".to_string(),
            });
        }
        let config = server.config().clone();
        let prev = std::mem::replace(
            server,
            McpServer::Connecting {
                config: config.clone(),
            },
        );
        let old_conn = match prev {
            McpServer::Connected { conn, .. } => Some(conn),
            _ => None,
        };
        Ok((config, old_conn))
    }

    /// Repasse un serveur en `Disconnected` ; renvoie la connexion à fermer (si une).
    pub fn begin_disconnect(&mut self, name: &str) -> Option<McpConnection> {
        let server = self.servers.get_mut(name)?;
        let config = server.config().clone();
        match std::mem::replace(server, McpServer::Disconnected { config }) {
            McpServer::Connected { conn, .. } => Some(conn),
            _ => None,
        }
    }

    /// Applique le succès d'une connexion. N'applique que si le serveur est
    /// toujours `Connecting` (sinon l'utilisateur a déconnecté entre-temps : la
    /// connexion est renvoyée à l'appelant pour fermeture).
    #[must_use = "la connexion renvoyée doit être fermée (cancel)"]
    pub fn finish_connect(
        &mut self,
        name: &str,
        conn: McpConnection,
        tools: Vec<McpToolInfo>,
    ) -> Option<McpConnection> {
        match self.servers.get_mut(name) {
            Some(server @ McpServer::Connecting { .. }) => {
                let config = server.config().clone();
                *server = McpServer::Connected {
                    config,
                    conn,
                    tools,
                };
                None
            }
            _ => Some(conn),
        }
    }

    /// Marque un serveur `Failed` (spawn ou handshake échoué). Sans effet si le
    /// serveur n'est plus `Connecting`.
    pub fn fail(&mut self, name: &str, error: String) {
        if let Some(server @ McpServer::Connecting { .. }) = self.servers.get_mut(name) {
            let config = server.config().clone();
            *server = McpServer::Failed { config, error };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpConfigFile;
    use std::collections::BTreeMap;

    fn registry_with(name: &str) -> McpRegistry {
        let mut servers = BTreeMap::new();
        servers.insert(
            name.to_string(),
            McpServerConfig {
                command: "echo".into(),
                args: Vec::new(),
                env: BTreeMap::new(),
                source: Default::default(),
                shadows_lower_priority: false,
            },
        );
        McpRegistry::from_config(McpConfigFile {
            servers,
            skipped: 0,
            issues: Vec::new(),
        })
    }

    #[test]
    fn begin_connect_rejects_when_already_connecting() {
        let mut reg = registry_with("srv");
        // 1er begin_connect : Disconnected → Connecting, aucune connexion précédente.
        let (_cfg, old) = reg.begin_connect("srv").unwrap();
        assert!(old.is_none());
        // 2e pendant Connecting → refusé (évite un double-spawn de process).
        assert!(reg.begin_connect("srv").is_err());
    }

    #[test]
    fn begin_connect_unknown_server_errs() {
        let mut reg = registry_with("srv");
        assert!(reg.begin_connect("absent").is_err());
    }

    #[test]
    fn begin_disconnect_resets_and_unblocks_connect() {
        let mut reg = registry_with("srv");
        reg.begin_connect("srv").unwrap();
        // En Connecting (pas Connected) → pas de connexion à fermer.
        assert!(reg.begin_disconnect("srv").is_none());
        // Retour Disconnected → begin_connect remarche.
        assert!(reg.begin_connect("srv").is_ok());
    }
}
