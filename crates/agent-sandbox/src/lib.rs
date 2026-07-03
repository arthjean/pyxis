//! `agent-sandbox` — bac à sable d'exécution (US-020). Deux protections
//! complémentaires :
//! - **FS** : confinement kernel-level via Landlock (`fs`), appliqué process-wide
//!   au démarrage → toute écriture est confinée au workspace (agent ET
//!   sous-process Bash hérités).
//! - **Réseau** : proxy CONNECT allow-list (`proxy`) ; les sous-process outils
//!   reçoivent `HTTP(S)_PROXY` → filtrage best-effort par hostname.
//!
//! Linux-first : hors Linux, le FS dégrade explicitement (AC3). Le proxy reste
//! disponible (pur tokio).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod fs;
pub mod proxy;

pub use fs::{SandboxError, SandboxStatus, enforce_process};
pub use proxy::{ProxyHandle, ProxyPolicy, spawn as spawn_proxy};

const SAFE_ENV_KEYS: &[&str] = &[
    "PATH",
    "Path",
    "HOME",
    "USER",
    "USERNAME",
    "USERPROFILE",
    "SystemRoot",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
    "TMPDIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "NO_COLOR",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Vrai pour les variables conservables dans les sous-process d'outils.
/// L'objectif est d'éviter l'héritage ambiant de secrets (`OPENAI_API_KEY`,
/// tokens cloud, credentials CI) tout en gardant PATH, home et certificats.
pub fn should_preserve_env_key(key: &str) -> bool {
    SAFE_ENV_KEYS.contains(&key)
}

/// Injecte l'environnement durci d'une commande d'outil ou MCP, sans toucher
/// l'environnement global du process. Le provider de l'agent continue d'appeler le
/// réseau en direct, tandis que les sous-process passent par le proxy filtrant.
pub fn set_proxy_env(cmd: &mut tokio::process::Command, proxy_addr: &str) {
    let preserved: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
        .filter(|(k, _)| k.to_str().is_some_and(should_preserve_env_key))
        .collect();
    let url = format!("http://{proxy_addr}");
    cmd.env_clear();
    for (k, v) in preserved {
        cmd.env(k, v);
    }
    cmd.env("HTTP_PROXY", &url)
        .env("HTTPS_PROXY", &url)
        .env("http_proxy", &url)
        .env("https_proxy", &url)
        .env("ALL_PROXY", &url)
        .env("all_proxy", &url)
        // Empêche les outils de bypasser le proxy pour localhost only si voulu.
        // NO_PROXY vide signifie que tout passe par le proxy filtrant.
        .env("NO_PROXY", "")
        .env("no_proxy", "");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_allowlist_keeps_runtime_basics_and_rejects_secrets() {
        assert!(should_preserve_env_key("PATH"));
        assert!(should_preserve_env_key("HOME"));
        assert!(should_preserve_env_key("SSL_CERT_FILE"));
        assert!(!should_preserve_env_key("OPENAI_API_KEY"));
        assert!(!should_preserve_env_key("AWS_SECRET_ACCESS_KEY"));
        assert!(!should_preserve_env_key("NO_PROXY"));
    }

    #[test]
    fn set_proxy_env_forces_all_proxy_variants() {
        let mut cmd = tokio::process::Command::new("tool");
        set_proxy_env(&mut cmd, "127.0.0.1:4242");
        let envs: std::collections::BTreeMap<_, _> = cmd
            .as_std()
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().to_string(),
                    v?.to_string_lossy().to_string(),
                ))
            })
            .collect();
        let url = "http://127.0.0.1:4242";
        assert_eq!(envs.get("HTTP_PROXY").map(String::as_str), Some(url));
        assert_eq!(envs.get("HTTPS_PROXY").map(String::as_str), Some(url));
        assert_eq!(envs.get("ALL_PROXY").map(String::as_str), Some(url));
        assert_eq!(envs.get("NO_PROXY").map(String::as_str), Some(""));
        #[cfg(not(windows))]
        {
            assert_eq!(envs.get("http_proxy").map(String::as_str), Some(url));
            assert_eq!(envs.get("https_proxy").map(String::as_str), Some(url));
            assert_eq!(envs.get("all_proxy").map(String::as_str), Some(url));
            assert_eq!(envs.get("no_proxy").map(String::as_str), Some(""));
        }
    }
}
