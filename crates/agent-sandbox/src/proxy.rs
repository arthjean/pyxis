//! Local CONNECT proxy with a hostname allow-list (US-020 AC2). Landlock does not
//! filter the network (ADR-7 R3) -> **best-effort** application-level filtering: the
//! tool subprocesses get `HTTP(S)_PROXY` pointing here; a client that
//! honors the variable for CONNECT tunnels is filtered. Fail-closed: any
//! hostname outside the allow-list is blocked (403) and reported. Non-CONNECT HTTP
//! requests are refused, not forwarded.
//!
//! Accepted best-effort: a binary that opens a raw socket while ignoring
//! `HTTP_PROXY` escapes the filter (the Landlock FS confinement, in contrast, stays hard).
//! Hard network confinement (Landlock AccessNet V4 / nftables) is deferred.
//!
//! US-003: network access is a property of the [`SandboxPolicy`], not an
//! independent setting, and an authorization covers the SUBDOMAINS of the host
//! it names, on a label boundary. US-004 adds one-call grants, the only
//! perimeter Pyxis can widen without restarting.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use agent_core::sandbox::SandboxPolicy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Reported to the user when the proxy blocks a host. The refusal is restituted,
/// not merely logged (US-003 AC6).
pub type ProxyNotice = Arc<dyn Fn(String) + Send + Sync>;

/// Network policy: hostname allow-list (fail-closed). Empty = no network
/// allowed for the tools (safe default).
#[derive(Debug, Clone, Default)]
pub struct ProxyPolicy {
    allow: Vec<String>,
    /// Carried by the sandbox policy (US-003 AC1). When false the allow-list is
    /// moot: the policy closed the network, and no list reopens it.
    network_access: bool,
}

impl ProxyPolicy {
    /// Allow-list with network access open: what `--allow` alone expresses.
    pub fn new(allow: Vec<String>) -> Self {
        Self {
            allow: allow.iter().map(|host| normalize_host(host)).collect(),
            network_access: true,
        }
    }

    /// Derives the network policy from the sandbox policy (US-003 AC1). Returns
    /// the notice to show when the two disagree: a closed policy WINS over the
    /// allow-list, deterministically, and says so (AC5, edge case #5).
    pub fn from_sandbox(policy: &SandboxPolicy, allow: Vec<String>) -> (Self, Option<String>) {
        if policy.network_access() {
            return (Self::new(allow), None);
        }
        let notice = (!allow.is_empty()).then(|| {
            format!(
                "network closed by the `{}` sandbox policy; the allow-list ({}) is ignored",
                policy.id(),
                allow.join(", ")
            )
        });
        (
            Self {
                allow: Vec::new(),
                network_access: false,
            },
            notice,
        )
    }

    /// Hosts the tools may reach, for the message shown on a refusal.
    pub fn allowed(&self) -> &[String] {
        &self.allow
    }

    /// Is `host` covered by the allow-list?
    ///
    /// Matching is on a DOMAIN LABEL boundary (US-003 AC3): allowing
    /// `github.com` covers `api.github.com` but never `evil-github.com` nor
    /// `github.com.evil.test`. And it only DESCENDS: allowing `api.github.com`
    /// does not allow `github.com` (AC4).
    pub fn is_allowed(&self, host: &str) -> bool {
        if !self.network_access {
            return false;
        }
        let host = normalize_host(host);
        self.allow
            .iter()
            .any(|allowed| is_suffix_match(&host, allowed))
    }
}

/// Case and trailing dot are not part of a host's identity; normalizing both
/// sides removes a class of allow-list bypass (`GitHub.com`, `github.com.`).
fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// `host` is `allowed` itself, or one of its subdomains. The character before
/// the suffix must be a dot, which is exactly what makes the test a label
/// boundary and not a substring (`evil-github.com` vs `github.com`).
fn is_suffix_match(host: &str, allowed: &str) -> bool {
    if allowed.is_empty() {
        return false;
    }
    if host == allowed {
        return true;
    }
    host.len() > allowed.len()
        && host.ends_with(allowed)
        && host.as_bytes()[host.len() - allowed.len() - 1] == b'.'
}

/// Hosts granted outside the policy: for the duration of ONE tool call (US-004)
/// or, once a human said so at the moment of the block, for the rest of the
/// session. Kept apart from the policy on purpose: neither must ever look like a
/// policy change, and both disappear with the process.
#[derive(Clone, Default)]
pub struct NetworkGrants {
    inner: Arc<Mutex<HashMap<String, usize>>>,
    /// Hosts a human allowed for the session, at a block, through
    /// [`NetworkApprover`]. Separate from the counted map because these have no
    /// guard: they are released by the process ending, and by nothing else.
    session: Arc<Mutex<Vec<String>>>,
}

impl NetworkGrants {
    /// Opens a grant for `host`. The widening lasts exactly as long as the
    /// returned guard (US-004 AC3): dropping it is what revokes the grant, so
    /// no code path can forget to.
    pub fn grant(&self, host: &str) -> NetworkGrant {
        let host = normalize_host(host);
        if let Ok(mut map) = self.inner.lock() {
            *map.entry(host.clone()).or_insert(0) += 1;
        }
        NetworkGrant {
            grants: self.clone(),
            host,
        }
    }

    /// Records a host a human allowed for the whole session. Stored as a
    /// suffix rule, exactly like the policy's own entries: allowing
    /// `github.com` at a block covers `api.github.com` afterwards, on a label
    /// boundary, and covers nothing that merely ends with the same characters.
    pub fn grant_for_session(&self, host: &str) {
        let host = normalize_host(host);
        if host.is_empty() {
            return;
        }
        let mut session = match self.session.lock() {
            Ok(session) => session,
            Err(poisoned) => poisoned.into_inner(),
        };
        if !session.iter().any(|granted| granted == &host) {
            session.push(host);
        }
    }

    /// Hosts allowed for the session so far, for display (`/sandbox`).
    pub fn session_grants(&self) -> Vec<String> {
        match self.session.lock() {
            Ok(session) => session.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn allows(&self, host: &str) -> bool {
        let once = match self.inner.lock() {
            Ok(map) => map.contains_key(host),
            Err(poisoned) => poisoned.into_inner().contains_key(host),
        };
        once || self.session_allows(host)
    }

    fn session_allows(&self, host: &str) -> bool {
        let session = match self.session.lock() {
            Ok(session) => session,
            Err(poisoned) => poisoned.into_inner(),
        };
        session.iter().any(|granted| is_suffix_match(host, granted))
    }

    fn release(&self, host: &str) {
        let Ok(mut map) = self.inner.lock() else {
            return;
        };
        if let Some(count) = map.get_mut(host) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(host);
            }
        }
    }
}

/// What a human answers when the proxy blocks a host and asks (US-004, ported
/// from Codex `core/src/tools/network_approval.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkDecision {
    /// Let this connection through, and nothing else.
    AllowOnce,
    /// Allow this host, and its subdomains, for the rest of the session.
    AllowSession,
    /// Keep the refusal.
    Deny,
}

/// Asked, at the moment of a block, whether to let a host through.
///
/// Deliberately consulted INSIDE the proxy rather than after a failed tool call:
/// a command that reaches the network usually fails in a way its own output does
/// not explain, and by the time the tool returns, the connection the user would
/// have allowed is already gone. Asking here means the answer can still save the
/// call in flight.
///
/// Fail-closed by construction: an absent approver, an approver that errors, and
/// an approver that does not answer within [`APPROVAL_TIMEOUT`] all keep the
/// refusal.
#[async_trait::async_trait]
pub trait NetworkApprover: Send + Sync {
    async fn approve(&self, host: &str, allowed: &str) -> NetworkDecision;
}

/// Longest a blocked connection waits for a human. Past it the refusal stands:
/// a TCP client that gets no answer at all is a worse failure mode than one that
/// gets a 403, and an unattended run must not hang on a question nobody reads.
pub const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// RAII token of a one-call network grant. Revokes on drop.
pub struct NetworkGrant {
    grants: NetworkGrants,
    host: String,
}

impl Drop for NetworkGrant {
    fn drop(&mut self) {
        self.grants.release(&self.host);
    }
}

/// Handle of a running proxy.
#[derive(Clone)]
pub struct ProxyHandle {
    /// `127.0.0.1:PORT` address to export as `HTTP(S)_PROXY`.
    pub addr: String,
    /// Log of the blocked hosts (AC2 "logged"), readable by the frontend. Append
    /// only: its length is the mark a caller takes around one tool call.
    pub blocked: Arc<Mutex<Vec<String>>>,
    /// One-call grants (US-004), shared with the running proxy.
    pub grants: NetworkGrants,
    /// Hosts the policy allows, for the message a refusal must carry.
    pub allowed: Vec<String>,
}

impl ProxyHandle {
    /// Hosts blocked since a previous [`ProxyHandle::mark`].
    pub fn blocked_since(&self, mark: usize) -> Vec<String> {
        let log = match self.blocked.lock() {
            Ok(log) => log,
            Err(poisoned) => poisoned.into_inner(),
        };
        log.get(mark..).map(<[String]>::to_vec).unwrap_or_default()
    }

    /// Opaque position in the block log, to be passed back to
    /// [`ProxyHandle::blocked_since`].
    pub fn mark(&self) -> usize {
        match self.blocked.lock() {
            Ok(log) => log.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }
}

/// Starts the proxy on a free local port. Returns its handle.
///
/// `notice` restitutes a refusal to the user (US-003 AC6). `None` keeps the
/// block in the log only, which is what the tests and the headless path want.
pub async fn spawn(
    policy: ProxyPolicy,
    notice: Option<ProxyNotice>,
) -> std::io::Result<ProxyHandle> {
    spawn_with_approver(policy, notice, None).await
}

/// Same, with a human consulted at each block (US-004). Without an approver the
/// behavior is byte-for-byte the historical one: a blocked host is logged,
/// restituted and refused, and nothing waits.
pub async fn spawn_with_approver(
    policy: ProxyPolicy,
    notice: Option<ProxyNotice>,
    approver: Option<Arc<dyn NetworkApprover>>,
) -> std::io::Result<ProxyHandle> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let blocked = Arc::new(Mutex::new(Vec::new()));
    let grants = NetworkGrants::default();
    let allowed = policy.allowed().to_vec();
    let policy = Arc::new(policy);

    let blocked_bg = Arc::clone(&blocked);
    let grants_bg = grants.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let policy = Arc::clone(&policy);
            let blocked = Arc::clone(&blocked_bg);
            let grants = grants_bg.clone();
            let notice = notice.clone();
            let approver = approver.clone();
            tokio::spawn(async move {
                let _ = handle_conn(sock, policy, grants, blocked, notice, approver).await;
            });
        }
    });

    Ok(ProxyHandle {
        addr,
        blocked,
        grants,
        allowed,
    })
}

async fn handle_conn(
    mut client: TcpStream,
    policy: Arc<ProxyPolicy>,
    grants: NetworkGrants,
    blocked: Arc<Mutex<Vec<String>>>,
    notice: Option<ProxyNotice>,
    approver: Option<Arc<dyn NetworkApprover>>,
) -> std::io::Result<()> {
    // Read the headers up to CRLFCRLF.
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

    // A one-call grant (US-004) is checked alongside the policy, never merged
    // into it: the policy of the session stays what the user configured.
    if !policy.is_allowed(&host) && !grants.allows(&normalize_host(&host)) {
        let allowed = describe_allowed(policy.allowed());
        // US-004: ask BEFORE recording the block. A host a human just allowed
        // was never blocked, so logging it would make the tool output claim a
        // refusal that did not happen, and would offer a retry for a connection
        // that already went through.
        let decision = match &approver {
            Some(approver) => decide(approver.as_ref(), &host, &allowed).await,
            None => NetworkDecision::Deny,
        };
        match decision {
            NetworkDecision::AllowSession => {
                grants.grant_for_session(&host);
                if let Some(notice) = &notice {
                    notice(format!(
                        "network allowed for this session: {host} (and its subdomains)"
                    ));
                }
            }
            NetworkDecision::AllowOnce => {
                if let Some(notice) = &notice {
                    notice(format!("network allowed once: {host}"));
                }
            }
            NetworkDecision::Deny => {
                if let Ok(mut log) = blocked.lock() {
                    log.push(host.clone());
                }
                // US-003 AC6: the refusal names the host AND the active
                // allow-list, on both channels -> the body reaches the model
                // through the tool output, the notice reaches the user.
                if let Some(notice) = notice {
                    notice(format!("network blocked: {host} (allowed: {allowed})"));
                }
                let body = format!(
                    "HTTP/1.1 403 Forbidden\r\n\r\nblocked by pyxis network allow-list: {host} (allowed: {allowed})"
                );
                let _ = client.write_all(body.as_bytes()).await;
                return Ok(());
            }
        }
    }

    // Allowed: real DNS resolution + bidirectional tunnel.
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

/// Consults the approver under a bound. A question that never comes back is a
/// refusal, so an unattended run degrades to the historical behavior instead of
/// holding a socket open forever.
async fn decide(approver: &dyn NetworkApprover, host: &str, allowed: &str) -> NetworkDecision {
    match tokio::time::timeout(APPROVAL_TIMEOUT, approver.approve(host, allowed)).await {
        Ok(decision) => decision,
        Err(_) => NetworkDecision::Deny,
    }
}

/// Renders the allow-list for a refusal message. An empty list is stated, not
/// left blank: "nothing is allowed" is the actual policy, not a missing value.
pub fn describe_allowed(allowed: &[String]) -> String {
    if allowed.is_empty() {
        "none".to_string()
    } else {
        allowed.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_fail_closed() {
        let p = ProxyPolicy::new(vec!["api.openai.com".to_string()]);
        assert!(p.is_allowed("api.openai.com"));
        assert!(!p.is_allowed("evil.test"));
        // no partial/suffix match (anti-bypass).
        assert!(!p.is_allowed("api.openai.com.evil.test"));
        // empty default = nothing allowed.
        assert!(!ProxyPolicy::default().is_allowed("anything"));
    }

    #[test]
    fn authorization_covers_subdomains_on_a_label_boundary() {
        // US-003 AC2/AC3: `github.com` covers its subdomains, and nothing that
        // merely ends with the same characters.
        let p = ProxyPolicy::new(vec!["github.com".to_string()]);
        assert!(p.is_allowed("github.com"));
        assert!(p.is_allowed("api.github.com"));
        assert!(p.is_allowed("raw.githubusercontent.com.github.com"));
        for refused in [
            "notgithub.com",
            "evil-github.com",
            "github.com.evil.test",
            "xgithub.com",
        ] {
            assert!(!p.is_allowed(refused), "{refused} must stay blocked");
        }
    }

    #[test]
    fn authorization_descends_but_never_climbs() {
        // US-003 AC4: allowing a subdomain says nothing about its parent.
        let p = ProxyPolicy::new(vec!["api.github.com".to_string()]);
        assert!(p.is_allowed("api.github.com"));
        assert!(p.is_allowed("v3.api.github.com"));
        assert!(!p.is_allowed("github.com"));
        assert!(!p.is_allowed("other.github.com"));
    }

    #[test]
    fn host_identity_ignores_case_and_trailing_dot() {
        let p = ProxyPolicy::new(vec!["GitHub.com.".to_string()]);
        assert!(p.is_allowed("api.GITHUB.com"));
        assert!(p.is_allowed("github.com."));
    }

    #[test]
    fn a_session_grant_covers_subdomains_but_not_lookalikes() {
        // US-004: a host allowed at a block is a suffix rule, exactly like a
        // policy entry. Anything else would let `evil-github.com` through on the
        // strength of a `github.com` the user allowed.
        let grants = NetworkGrants::default();
        grants.grant_for_session("GitHub.com.");
        assert!(grants.allows("github.com"));
        assert!(grants.allows("api.github.com"));
        for refused in ["evil-github.com", "github.com.evil.test", "notgithub.com"] {
            assert!(!grants.allows(refused), "{refused} must stay blocked");
        }
    }

    #[test]
    fn a_one_call_grant_still_dies_with_its_guard_while_session_grants_persist() {
        let grants = NetworkGrants::default();
        {
            let _guard = grants.grant("once.test");
            assert!(grants.allows("once.test"));
        }
        assert!(
            !grants.allows("once.test"),
            "a one-call grant must not survive its guard"
        );
        grants.grant_for_session("kept.test");
        assert_eq!(grants.session_grants(), vec!["kept.test".to_string()]);
    }

    #[tokio::test]
    async fn an_approver_that_never_answers_keeps_the_refusal() {
        // Fail-closed: the bound is what stops an unattended run from hanging on
        // a question nobody is there to read.
        struct Silent;
        #[async_trait::async_trait]
        impl NetworkApprover for Silent {
            async fn approve(&self, _host: &str, _allowed: &str) -> NetworkDecision {
                std::future::pending::<NetworkDecision>().await
            }
        }
        tokio::time::pause();
        let decision = decide(&Silent, "slow.test", "none");
        tokio::pin!(decision);
        // Nothing is decided before the bound elapses.
        assert!(
            futures_poll(&mut decision).is_none(),
            "the decision must wait for the approver"
        );
        tokio::time::advance(APPROVAL_TIMEOUT + std::time::Duration::from_secs(1)).await;
        assert_eq!(decision.await, NetworkDecision::Deny);
    }

    /// Polls a pinned future once without blocking, to assert it is still
    /// pending. `tokio::time::pause` makes the advance deterministic.
    fn futures_poll<F: std::future::Future>(
        future: &mut std::pin::Pin<&mut F>,
    ) -> Option<F::Output> {
        use std::task::{Context, Poll, Waker};
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => Some(value),
            Poll::Pending => None,
        }
    }

    #[test]
    fn a_closed_policy_wins_over_the_allowlist() {
        // US-003 AC5 / edge case #5: deterministic resolution, announced.
        let read_only = SandboxPolicy::ReadOnly {
            network_access: false,
        };
        let (policy, notice) =
            ProxyPolicy::from_sandbox(&read_only, vec!["github.com".to_string()]);
        assert!(!policy.is_allowed("github.com"));
        assert!(policy.allowed().is_empty());
        let notice = notice.expect("the conflict must be announced");
        assert!(notice.contains("github.com"), "{notice}");
        assert!(notice.contains("read-only"), "{notice}");
    }

    #[test]
    fn an_open_policy_keeps_the_allowlist_and_says_nothing() {
        let open = SandboxPolicy::workspace_write("/w", Vec::new(), [".git"]);
        let (policy, notice) = ProxyPolicy::from_sandbox(&open, vec!["github.com".to_string()]);
        assert!(policy.is_allowed("api.github.com"));
        assert!(notice.is_none());
    }

    #[test]
    fn a_grant_lasts_exactly_as_long_as_its_guard() {
        // US-004 AC3: the widening does not survive the call.
        let grants = NetworkGrants::default();
        assert!(!grants.allows("example.test"));
        {
            let _guard = grants.grant("Example.test");
            assert!(grants.allows("example.test"));
        }
        assert!(!grants.allows("example.test"));
    }

    #[test]
    fn nested_grants_release_one_at_a_time() {
        let grants = NetworkGrants::default();
        let outer = grants.grant("example.test");
        {
            let _inner = grants.grant("example.test");
            assert!(grants.allows("example.test"));
        }
        assert!(grants.allows("example.test"), "the outer grant still holds");
        drop(outer);
        assert!(!grants.allows("example.test"));
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
        let handle = spawn(ProxyPolicy::new(vec!["example.com".to_string()]), None)
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

    // US-020 AC2: allowed host tunneled; forbidden host -> 403 + logged.
    #[tokio::test]
    async fn allowed_tunnels_blocked_403_and_logged() {
        let upstream = local_upstream().await;
        let port = upstream.split(':').nth(1).unwrap().to_string();
        // we allow 127.0.0.1 (resolved locally to the upstream).
        let handle = spawn(ProxyPolicy::new(vec!["127.0.0.1".to_string()]), None)
            .await
            .unwrap();

        let (ok, body) = connect_through(&handle.addr, &format!("127.0.0.1:{port}")).await;
        assert!(ok.contains("200"), "autorisé non tunnelisé: {ok}");
        assert!(body.contains("UP-OK"), "bannière upstream absente");

        let (blocked, _) = connect_through(&handle.addr, "evil.exfil.test:443").await;
        assert!(blocked.contains("403"), "interdit non bloqué: {blocked}");

        // logging of the block (AC2).
        let log = handle.blocked.lock().unwrap();
        assert!(
            log.iter().any(|h| h == "evil.exfil.test"),
            "blocage non journalisé: {log:?}"
        );
    }

    #[tokio::test]
    async fn a_refusal_names_the_host_and_the_active_allowlist() {
        // US-003 AC6: restituted, not merely logged.
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let notice: ProxyNotice = Arc::new(move |message| {
            if let Ok(mut log) = sink.lock() {
                log.push(message);
            }
        });
        let handle = spawn(
            ProxyPolicy::new(vec!["github.com".to_string()]),
            Some(notice),
        )
        .await
        .unwrap();

        let (_status, body) = connect_through(&handle.addr, "evil-github.com:443").await;
        assert!(body.contains("evil-github.com"), "{body}");
        assert!(body.contains("github.com"), "{body}");

        let seen = seen.lock().unwrap();
        assert!(
            seen.iter()
                .any(|m| m.contains("evil-github.com") && m.contains("allowed: github.com")),
            "refus non restitué: {seen:?}"
        );
    }

    #[tokio::test]
    async fn a_one_call_grant_opens_then_closes_the_tunnel() {
        // US-004 AC2/AC3: the escalation opens the host for one call only.
        let upstream = local_upstream().await;
        let port = upstream.split(':').nth(1).unwrap().to_string();
        let handle = spawn(ProxyPolicy::new(Vec::new()), None).await.unwrap();
        let target = format!("127.0.0.1:{port}");

        let (before, _) = connect_through(&handle.addr, &target).await;
        assert!(before.contains("403"), "sans grant: {before}");
        {
            let _grant = handle.grants.grant("127.0.0.1");
            let (during, body) = connect_through(&handle.addr, &target).await;
            assert!(during.contains("200"), "avec grant: {during}");
            assert!(body.contains("UP-OK"));
        }
        let (after, _) = connect_through(&handle.addr, &target).await;
        assert!(after.contains("403"), "après le grant: {after}");
    }
}
