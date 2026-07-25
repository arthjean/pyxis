//! Local CONNECT proxy with a hostname allow-list (best-effort application-level
//! network filtering: Landlock cannot do it, see ADR-7 R3).
//!
//! `demo()` is self-contained and deterministic (no Internet access required):
//! it brings up a local TCP upstream, routes an allowed host through the proxy
//! (tunnel established) and a forbidden host (403 + logging), then asserts both
//! outcomes. DNS resolution is stubbed through `resolve` for reproducibility; a
//! real proxy would resolve through the system DNS. The security logic (the
//! allow-list check on the requested hostname) is identical either way.

use anyhow::{Result, bail};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct ProxyConfig {
    /// Explicitly allowed hostnames (fail-closed: everything else is blocked).
    pub allow: Vec<String>,
    /// host -> concrete addr. DNS stubbed for the self-contained demo.
    pub resolve: HashMap<String, String>,
}

impl ProxyConfig {
    fn is_allowed(&self, host: &str) -> bool {
        self.allow.iter().any(|h| h == host)
    }
}

/// Handles a client connection: reads the CONNECT request, applies the allow-list,
/// tunnels when allowed, returns a 403 otherwise.
async fn handle_conn(mut client: TcpStream, cfg: Arc<ProxyConfig>) -> Result<()> {
    // Read up to the end of the headers (CRLFCRLF).
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            bail!("requête proxy anormalement longue");
        }
    }

    let head = String::from_utf8_lossy(&buf);
    let first = head.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or(""); // host:port

    if method != "CONNECT" {
        client
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
            .await?;
        return Ok(());
    }

    let host = target.split(':').next().unwrap_or(target).to_string();
    let port = target.split(':').nth(1).unwrap_or("443").to_string();

    if !cfg.is_allowed(&host) {
        eprintln!(
            "[proxy] BLOQUÉ  host={host} (hors allow-list {:?}) — 403",
            cfg.allow
        );
        client
            .write_all(b"HTTP/1.1 403 Forbidden\r\n\r\nblocked by pyxis allow-list")
            .await?;
        return Ok(());
    }

    let upstream_addr = cfg
        .resolve
        .get(&host)
        .cloned()
        .unwrap_or_else(|| format!("{host}:{port}"));
    eprintln!("[proxy] AUTORISÉ host={host} -> {upstream_addr} — tunnel établi");

    let mut upstream = TcpStream::connect(&upstream_addr).await?;
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Small TCP upstream: announces a known banner as soon as a client connects.
async fn spawn_upstream() -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let _ = sock.write_all(b"UPSTREAM-OK\n").await;
            let _ = sock.flush().await;
        }
    });
    Ok(addr)
}

async fn spawn_proxy(cfg: Arc<ProxyConfig>) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let cfg = Arc::clone(&cfg);
            tokio::spawn(async move {
                if let Err(e) = handle_conn(sock, cfg).await {
                    eprintln!("[proxy] conn error: {e}");
                }
            });
        }
    });
    Ok(addr)
}

/// Sends a CONNECT request to the proxy and returns (status line, received body).
async fn connect_through(proxy_addr: &str, target_host: &str) -> Result<(String, String)> {
    let mut s = TcpStream::connect(proxy_addr).await?;
    let req = format!("CONNECT {target_host}:443 HTTP/1.1\r\nHost: {target_host}\r\n\r\n");
    s.write_all(req.as_bytes()).await?;

    let mut out = Vec::new();
    let mut tmp = [0u8; 512];
    // A few reads are enough for the demo (status + possible upstream banner).
    for _ in 0..4 {
        match tokio::time::timeout(std::time::Duration::from_millis(400), s.read(&mut tmp)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => out.extend_from_slice(&tmp[..n]),
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => break, // timeout: we read what there was
        }
    }
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.lines().next().unwrap_or("").to_string();
    Ok((status, text))
}

pub async fn demo() -> Result<()> {
    let upstream = spawn_upstream().await?;
    let mut resolve = HashMap::new();
    resolve.insert("api.allowed.test".to_string(), upstream.clone());

    let cfg = Arc::new(ProxyConfig {
        allow: vec!["api.allowed.test".to_string()],
        resolve,
    });
    let proxy_addr = spawn_proxy(Arc::clone(&cfg)).await?;
    println!(
        "[proxy] proxy={proxy_addr}  upstream={upstream}  allow={:?}",
        cfg.allow
    );

    // Case 1: allowed host -> tunnel established + upstream banner.
    let (status_ok, body_ok) = connect_through(&proxy_addr, "api.allowed.test").await?;
    println!("[proxy] autorisé  -> status={status_ok:?}");
    if !status_ok.contains("200") {
        bail!("hôte autorisé non tunnelisé : {status_ok:?}");
    }
    if !body_ok.contains("UPSTREAM-OK") {
        bail!("tunnel établi mais bannière upstream absente — copie bidirectionnelle KO");
    }

    // Case 2: forbidden host -> 403, never an upstream connection (unhappy path).
    let (status_blocked, _) = connect_through(&proxy_addr, "evil.exfil.test").await?;
    println!("[proxy] interdit  -> status={status_blocked:?}");
    if !status_blocked.contains("403") {
        bail!("hôte interdit NON bloqué : {status_blocked:?} — allow-list inopérante");
    }

    println!(
        "[proxy] VERDICT: filtrage réseau par allow-list faisable en solo (proxy CONNECT applicatif)."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn allowed_host_tunnels_and_blocked_host_403() {
        let upstream = spawn_upstream().await.unwrap();
        let mut resolve = HashMap::new();
        resolve.insert("api.allowed.test".to_string(), upstream);
        let cfg = Arc::new(ProxyConfig {
            allow: vec!["api.allowed.test".to_string()],
            resolve,
        });
        let proxy_addr = spawn_proxy(cfg).await.unwrap();

        let (ok, body) = connect_through(&proxy_addr, "api.allowed.test")
            .await
            .unwrap();
        assert!(ok.contains("200"), "status autorisé inattendu: {ok}");
        assert!(body.contains("UPSTREAM-OK"), "bannière upstream absente");

        let (blocked, _) = connect_through(&proxy_addr, "evil.exfil.test")
            .await
            .unwrap();
        assert!(
            blocked.contains("403"),
            "hôte interdit non bloqué: {blocked}"
        );
    }

    #[test]
    fn allowlist_is_fail_closed() {
        let cfg = ProxyConfig {
            allow: vec!["good.test".to_string()],
            resolve: HashMap::new(),
        };
        assert!(cfg.is_allowed("good.test"));
        assert!(!cfg.is_allowed("evil.test"));
        assert!(!cfg.is_allowed("good.test.evil.test"));
    }
}
