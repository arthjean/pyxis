//! Proxy CONNECT local avec allow-list de hostnames (US-020 AC2). Landlock ne
//! filtre pas le réseau (ADR-7 R3) → filtrage applicatif **best-effort** : les
//! sous-process outils reçoivent `HTTP(S)_PROXY` pointant ici ; un client qui
//! respecte la variable pour les tunnels CONNECT est filtré. Fail-closed : tout
//! hostname hors allow-list est bloqué (403) et journalisé. Les requêtes HTTP
//! non-CONNECT sont refusées, pas forwardées.
//!
//! Best-effort assumé : un binaire qui ouvre un socket brut en ignorant
//! `HTTP_PROXY` échappe au filtre (le confinement FS Landlock reste, lui, dur).
//! Le confinement réseau dur (Landlock AccessNet V4 / nftables) est différé.

use std::sync::Arc;
use std::sync::Mutex;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Politique réseau : allow-list de hostnames (fail-closed). Vide = aucun réseau
/// autorisé pour les outils (défaut sûr).
#[derive(Debug, Clone, Default)]
pub struct ProxyPolicy {
    pub allow: Vec<String>,
}

impl ProxyPolicy {
    pub fn new(allow: Vec<String>) -> Self {
        Self { allow }
    }
    pub fn is_allowed(&self, host: &str) -> bool {
        self.allow.iter().any(|h| h == host)
    }
}

/// Poignée d'un proxy en cours d'exécution.
#[derive(Clone)]
pub struct ProxyHandle {
    /// Adresse `127.0.0.1:PORT` à exporter en `HTTP(S)_PROXY`.
    pub addr: String,
    /// Journal des hôtes bloqués (AC2 « journalisé »), lisible par le frontend.
    pub blocked: Arc<Mutex<Vec<String>>>,
}

/// Démarre le proxy sur un port local libre. Retourne sa poignée.
pub async fn spawn(policy: ProxyPolicy) -> std::io::Result<ProxyHandle> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let blocked = Arc::new(Mutex::new(Vec::new()));
    let policy = Arc::new(policy);

    let blocked_bg = Arc::clone(&blocked);
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let policy = Arc::clone(&policy);
            let blocked = Arc::clone(&blocked_bg);
            tokio::spawn(async move {
                let _ = handle_conn(sock, policy, blocked).await;
            });
        }
    });

    Ok(ProxyHandle { addr, blocked })
}

async fn handle_conn(
    mut client: TcpStream,
    policy: Arc<ProxyPolicy>,
    blocked: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    // Lire les en-têtes jusqu'à CRLFCRLF.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 8192 {
            break;
        }
    }

    let head = String::from_utf8_lossy(&buf);
    let first = head.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or(""); // host:port

    if method != "CONNECT" {
        let _ = client
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
            .await;
        return Ok(());
    }

    let host = target.split(':').next().unwrap_or(target).to_string();
    let port = target.split(':').nth(1).unwrap_or("443");

    if !policy.is_allowed(&host) {
        if let Ok(mut log) = blocked.lock() {
            log.push(host.clone());
        }
        let _ = client
            .write_all(b"HTTP/1.1 403 Forbidden\r\n\r\nblocked by pyxis network allow-list")
            .await;
        return Ok(());
    }

    // Autorisé : résolution DNS réelle + tunnel bidirectionnel.
    let mut upstream = match TcpStream::connect(format!("{host}:{port}")).await {
        Ok(s) => s,
        Err(_) => {
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
            return Ok(());
        }
    };
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_fail_closed() {
        let p = ProxyPolicy::new(vec!["api.openai.com".to_string()]);
        assert!(p.is_allowed("api.openai.com"));
        assert!(!p.is_allowed("evil.test"));
        // pas de match partiel/suffixe (anti-contournement).
        assert!(!p.is_allowed("api.openai.com.evil.test"));
        // défaut vide = rien autorisé.
        assert!(!ProxyPolicy::default().is_allowed("anything"));
    }

    async fn local_upstream() -> String {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let _ = s.write_all(b"UP-OK\n").await;
                let _ = s.flush().await;
            }
        });
        addr
    }

    async fn connect_through(proxy: &str, target: &str) -> (String, String) {
        let mut s = TcpStream::connect(proxy).await.unwrap();
        let req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let mut tmp = [0u8; 256];
        for _ in 0..4 {
            match tokio::time::timeout(std::time::Duration::from_millis(300), s.read(&mut tmp))
                .await
            {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => out.extend_from_slice(&tmp[..n]),
                Ok(Err(_)) => break,
            }
        }
        let text = String::from_utf8_lossy(&out).to_string();
        let status = text.lines().next().unwrap_or("").to_string();
        (status, text)
    }

    #[tokio::test]
    async fn non_connect_requests_are_rejected() {
        let handle = spawn(ProxyPolicy::new(vec!["example.com".to_string()]))
            .await
            .unwrap();
        let mut s = TcpStream::connect(&handle.addr).await.unwrap();
        s.write_all(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .await
            .unwrap();
        let mut out = [0u8; 128];
        let n = s.read(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out[..n]);
        assert!(
            text.starts_with("HTTP/1.1 405 Method Not Allowed"),
            "non-CONNECT accepté: {text}"
        );
    }

    // US-020 AC2 : hôte autorisé tunnelisé ; hôte interdit → 403 + journalisé.
    #[tokio::test]
    async fn allowed_tunnels_blocked_403_and_logged() {
        let upstream = local_upstream().await;
        let port = upstream.split(':').nth(1).unwrap().to_string();
        // on autorise 127.0.0.1 (résolu localement vers l'upstream).
        let handle = spawn(ProxyPolicy::new(vec!["127.0.0.1".to_string()]))
            .await
            .unwrap();

        let (ok, body) = connect_through(&handle.addr, &format!("127.0.0.1:{port}")).await;
        assert!(ok.contains("200"), "autorisé non tunnelisé: {ok}");
        assert!(body.contains("UP-OK"), "bannière upstream absente");

        let (blocked, _) = connect_through(&handle.addr, "evil.exfil.test:443").await;
        assert!(blocked.contains("403"), "interdit non bloqué: {blocked}");

        // journalisation du blocage (AC2).
        let log = handle.blocked.lock().unwrap();
        assert!(
            log.iter().any(|h| h == "evil.exfil.test"),
            "blocage non journalisé: {log:?}"
        );
    }
}
