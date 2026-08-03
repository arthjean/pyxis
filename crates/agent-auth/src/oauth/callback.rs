//! The loopback redirect every authorization-code flow comes back through.
//!
//! Both flows in this crate bind a local port, open a browser, and wait for the
//! authorization server to redirect to it. They used to do that with two copies
//! of the same accept loop, and the copies drifted: only one of them checked the
//! request method, and only one of them noticed `?error=access_denied`. The
//! flow that did not would answer the user's refusal with a `404`, keep
//! listening, and hang until its own timeout expired.
//!
//! One loop, parameterized by the path it answers on and the page it shows.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::error::AuthError;
use crate::Secret;

/// A silent socket must not hold the flow: the browser has already been opened
/// and the real redirect is one connection away.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// A redirect is a request line and a few headers. Anything longer is not ours.
const MAX_REQUEST_BYTES: usize = 4096;

/// The local endpoint one flow is waiting on.
pub(super) struct Callback<'a> {
    /// Path this flow answers on. Anything else gets a `404`.
    pub path: &'a str,
    /// Headline of the page shown once the code has been captured.
    pub headline: &'a str,
    /// The anti-CSRF `state` this flow sent, and the only one it will accept.
    pub expected_state: &'a str,
}

impl Callback<'_> {
    /// Extracts the authorization code from a callback request line.
    ///
    /// `Err(Callback)` means "not our callback": the listener answers `404` and
    /// keeps waiting. Every other error ends the flow.
    pub(super) fn parse_request_line(&self, line: &str) -> Result<Secret, AuthError> {
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        if method != "GET" {
            return Err(AuthError::Callback(format!("not a GET: {method}")));
        }
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path != self.path {
            return Err(AuthError::Callback(format!("unexpected path {path}")));
        }

        let mut code = None;
        let mut state = None;
        let mut error = None;
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "code" => code = Some(value.into_owned()),
                "state" => state = Some(value.into_owned()),
                "error" => error = Some(value.into_owned()),
                _ => {}
            }
        }

        // A declined consent is an answer, not a stray request: reporting it
        // beats waiting out the flow's timeout on a redirect that already came.
        if let Some(error) = error {
            return Err(AuthError::AuthorizationDenied(error));
        }
        // The state is checked BEFORE the code is looked at: a callback that
        // does not belong to this flow must not get its code read or exchanged.
        if state.as_deref() != Some(self.expected_state) {
            return Err(AuthError::StateMismatch);
        }
        code.map(Secret::new)
            .ok_or_else(|| AuthError::Callback("callback carries no code".to_string()))
    }

    /// Accepts connections until this flow's callback arrives.
    pub(super) async fn accept(&self, listener: &TcpListener) -> Result<Secret, AuthError> {
        self.accept_with_read_timeout(listener, READ_TIMEOUT).await
    }

    async fn accept_with_read_timeout(
        &self,
        listener: &TcpListener,
        read_timeout: Duration,
    ) -> Result<Secret, AuthError> {
        loop {
            let (mut sock, _) = listener.accept().await?;
            let mut buf = [0_u8; MAX_REQUEST_BYTES];
            let read = tokio::time::timeout(read_timeout, sock.read(&mut buf)).await;
            let n = match read {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(e.into()),
                // A socket that opened and said nothing is not the redirect.
                Err(_) => {
                    respond(&mut sock, "408 Request Timeout", None).await;
                    continue;
                }
            };
            let request = String::from_utf8_lossy(&buf[..n]);
            match self.parse_request_line(request.lines().next().unwrap_or_default()) {
                Ok(code) => {
                    respond(&mut sock, "200 OK", Some(&success_page(self.headline))).await;
                    return Ok(code);
                }
                // Not our callback (favicon, probe): answer and keep waiting.
                Err(AuthError::Callback(_)) => {
                    respond(&mut sock, "404 Not Found", None).await;
                }
                // A mismatched state or an explicit refusal ends the flow.
                Err(err) => {
                    respond(&mut sock, "400 Bad Request", None).await;
                    return Err(err);
                }
            }
        }
    }
}

fn success_page(headline: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><body style=\"font-family:system-ui;background:#0b0b0b;color:#eaeaea;display:grid;place-items:center;height:100vh\"><div><h2>{headline}</h2><p>You can close this tab.</p></div></body>"
    )
}

/// Writes one response and lets the socket close. Failures are ignored on
/// purpose: the browser tab is a courtesy, the code is already captured.
async fn respond(sock: &mut tokio::net::TcpStream, status: &str, body: Option<&str>) {
    let response = match body {
        Some(body) => format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
        None => format!("HTTP/1.1 {status}\r\nConnection: close\r\n\r\n"),
    };
    let _ = sock.write_all(response.as_bytes()).await;
    let _ = sock.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callback(state: &'static str) -> Callback<'static> {
        Callback {
            path: "/auth/callback",
            headline: "Pyxis connected",
            expected_state: state,
        }
    }

    #[test]
    fn the_state_is_checked_before_the_code_is_read() {
        let line = "GET /auth/callback?code=CODE_SECRET&state=good HTTP/1.1";
        assert_eq!(
            callback("good").parse_request_line(line).unwrap().expose(),
            "CODE_SECRET"
        );
        let err = callback("other").parse_request_line(line).unwrap_err();
        assert!(matches!(err, AuthError::StateMismatch));
        assert!(!err.to_string().contains("CODE_SECRET"));
    }

    /// The bug the two copies of this loop disagreed on: a declined consent
    /// used to read as "not our callback" on the ChatGPT side, so the listener
    /// answered 404 and waited out its full timeout.
    #[test]
    fn a_declined_consent_ends_the_flow_instead_of_being_ignored() {
        let denied = "GET /auth/callback?error=access_denied&state=good HTTP/1.1";
        let err = callback("good").parse_request_line(denied).unwrap_err();
        assert!(matches!(err, AuthError::AuthorizationDenied(ref e) if e == "access_denied"));
        // Not a `Callback` error, which is what keeps the listener looping.
        assert!(!matches!(err, AuthError::Callback(_)));
    }

    #[test]
    fn anything_that_is_not_this_flows_callback_keeps_the_listener_waiting() {
        for line in [
            "GET /favicon.ico HTTP/1.1",
            "GET /auth/callback2?code=c&state=good HTTP/1.1",
            "GET /auth/callback/extra?code=c&state=good HTTP/1.1",
            "POST /auth/callback?code=c&state=good HTTP/1.1",
            "",
        ] {
            assert!(
                matches!(
                    callback("good").parse_request_line(line),
                    Err(AuthError::Callback(_))
                ),
                "{line}"
            );
        }
    }

    #[test]
    fn a_callback_without_a_code_is_not_a_callback() {
        assert!(matches!(
            callback("good").parse_request_line("GET /auth/callback?state=good HTTP/1.1"),
            Err(AuthError::Callback(_))
        ));
    }

    #[tokio::test]
    async fn a_silent_socket_does_not_hold_the_listener() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            callback("s1")
                .accept_with_read_timeout(&listener, Duration::from_millis(20))
                .await
        });

        let _silent = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut good = tokio::net::TcpStream::connect(addr).await.unwrap();
        good.write_all(b"GET /auth/callback?code=abc123&state=s1 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();

        let code = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(code.expose(), "abc123");
    }
}
