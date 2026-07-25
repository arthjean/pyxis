//! `agent-sandbox`: execution sandbox (US-020). Two complementary
//! protections:
//! - **FS**: kernel-level confinement through Landlock (`fs`), applied process-wide
//!   at startup -> every write is confined to the workspace (agent AND
//!   inherited Bash subprocesses).
//! - **Network**: allow-list CONNECT proxy (`proxy`); the tool subprocesses
//!   get `HTTP(S)_PROXY` -> best-effort filtering by hostname.
//!
//! Linux-first: outside Linux, the FS part degrades explicitly (AC3). The proxy stays
//! available (pure tokio).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod fs;
pub mod proxy;

pub use fs::{
    IgnoreReason, IgnoredRoot, SandboxError, SandboxStatus, WritableRoots, enforce_process,
    resolve_writable_roots,
};
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

/// True for the variables that can be kept in the tool subprocesses.
/// The goal is to avoid the ambient inheritance of secrets (`OPENAI_API_KEY`,
/// cloud tokens, CI credentials) while keeping PATH, home and certificates.
pub fn should_preserve_env_key(key: &str) -> bool {
    SAFE_ENV_KEYS.contains(&key)
}

/// Injects the hardened environment of a tool or MCP command, without touching
/// the global process environment. The agent provider keeps calling the
/// network directly, while the subprocesses go through the filtering proxy.
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
        // Prevents the tools from bypassing the proxy for localhost only when wanted.
        // An empty NO_PROXY means everything goes through the filtering proxy.
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
