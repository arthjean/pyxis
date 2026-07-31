//! Finding the authorization server that guards an MCP endpoint, under RFC 9728
//! then RFC 8414.
//!
//! Discovery is the part of the flow that talks to a party which has not
//! authenticated yet and gets to name the URLs everything after it will use, so
//! it carries the refusals rather than the caller:
//!
//! 1. **Every discovered endpoint must be https**, loopback aside. A
//!    `http://evil.example` handed back here would receive the authorization
//!    code in clear.
//! 2. **The authorization server must be named by the protected-resource
//!    document**, never by a `WWW-Authenticate` header alone.
//! 3. **The document must claim the resource it protects** (RFC 9728 §3.3).
//!    Without this the RFC 8707 binding is chosen by the server: it names a
//!    third party's resource, the user consents at that party's real identity
//!    provider, and the token comes back to whoever asked for it.
//! 4. **Every body is read under a cap**, before it is allocated rather than
//!    after.

use super::{AuthServerMetadata, HTTP_TIMEOUT, MAX_METADATA_BYTES};
use crate::oauth::openai_chatgpt::AuthError;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    authorization_servers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawAuthServerMetadata {
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    authorization_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint: Option<String>,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

/// Reads the `resource_metadata` hint out of a `WWW-Authenticate` header.
///
/// The hint is only ever a *shortcut* to the protected-resource document; the
/// authorization server still has to come from that document (invariant 2).
pub fn resource_metadata_hint(www_authenticate: &str) -> Option<String> {
    let (_, rest) = www_authenticate.split_once("resource_metadata")?;
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let value = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => rest.split([',', ' ']).next()?,
    };
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Candidate protected-resource metadata URLs for an MCP endpoint, in the order
/// the spec expects them to be tried.
fn protected_resource_urls(mcp_url: &url::Url) -> Vec<String> {
    let origin = format!(
        "{}://{}",
        mcp_url.scheme(),
        mcp_url.authority().trim_start_matches('@')
    );
    let path = mcp_url.path().trim_end_matches('/');
    let mut candidates = Vec::new();
    if !path.is_empty() {
        // Path-aware form first: one origin can host several MCP resources.
        candidates.push(format!(
            "{origin}/.well-known/oauth-protected-resource{path}"
        ));
    }
    candidates.push(format!("{origin}/.well-known/oauth-protected-resource"));
    candidates
}

/// Candidate authorization-server metadata URLs for an issuer.
fn auth_server_urls(issuer: &url::Url) -> Vec<String> {
    let origin = format!(
        "{}://{}",
        issuer.scheme(),
        issuer.authority().trim_start_matches('@')
    );
    let path = issuer.path().trim_end_matches('/');
    let mut candidates = Vec::new();
    if !path.is_empty() {
        candidates.push(format!(
            "{origin}/.well-known/oauth-authorization-server{path}"
        ));
        candidates.push(format!("{origin}{path}/.well-known/openid-configuration"));
    }
    candidates.push(format!("{origin}/.well-known/oauth-authorization-server"));
    candidates.push(format!("{origin}/.well-known/openid-configuration"));
    candidates
}

/// Refuses any endpoint that is not encrypted. Plain http is allowed for a
/// loopback host only, which is where every local MCP server in development lives.
pub(super) fn require_secure(url: &str, what: &str) -> Result<url::Url, AuthError> {
    let parsed = url::Url::parse(url)
        .map_err(|e| AuthError::Callback(format!("{what}: invalid url \"{url}\": {e}")))?;
    let secure = match parsed.scheme() {
        "https" => true,
        "http" => parsed.host_str().is_some_and(is_loopback_host),
        _ => false,
    };
    if secure {
        Ok(parsed)
    } else {
        Err(AuthError::Callback(format!(
            "{what}: refused, {url} is not https (only a loopback host may be plain http)"
        )))
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Reads a body under a cap, chunk by chunk, refusing as soon as the cap is
/// passed rather than after the whole answer has been allocated. `Content-Length`
/// is only an early exit: the cap is enforced on what actually arrives, so a
/// chunked or lying response is bounded all the same.
async fn read_bounded(mut response: reqwest::Response, max: usize) -> Result<Vec<u8>, AuthError> {
    let too_large = || AuthError::Callback(format!("response larger than {max} bytes"));
    if response
        .content_length()
        .is_some_and(|len| len > max as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > max {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Bounded read + parse of a response this flow needs to succeed on.
pub(super) async fn read_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    max: usize,
) -> Result<T, AuthError> {
    let body = tokio::time::timeout(HTTP_TIMEOUT, read_bounded(response, max))
        .await
        .map_err(|_| AuthError::Callback("response body timed out".to_string()))??;
    serde_json::from_slice(&body)
        .map_err(|e| AuthError::TokenResponse(format!("malformed response: {e}")))
}

/// Bounded read + parse of a discovery document. Every failure reads the same
/// way: this candidate is not usable, try the next one.
async fn fetch_metadata<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Option<T> {
    let response = tokio::time::timeout(HTTP_TIMEOUT, client.get(url).send())
        .await
        .ok()?
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    read_json(response, MAX_METADATA_BYTES).await.ok()
}

/// Discovers the authorization server guarding `mcp_url`.
///
/// `hint` is the `resource_metadata` URL a `401` may have named. It is tried
/// first, but it is checked like any other endpoint and it can only lead to a
/// protected-resource document, never straight to an authorization server.
pub async fn discover(
    client: &reqwest::Client,
    mcp_url: &str,
    hint: Option<&str>,
) -> Result<(AuthServerMetadata, Option<String>), AuthError> {
    let parsed = require_secure(mcp_url, "MCP endpoint")?;
    let mut candidates = Vec::new();
    if let Some(hint) = hint {
        // A hint pointing elsewhere than the resource origin is refused: it would
        // let a compromised server delegate its own protection to a third party.
        let hint_url = require_secure(hint, "resource metadata")?;
        if hint_url.host_str() == parsed.host_str() {
            candidates.push(hint.to_string());
        }
    }
    candidates.extend(protected_resource_urls(&parsed));

    let mut resource = None;
    let mut issuers: Vec<String> = Vec::new();
    for candidate in &candidates {
        let Some(metadata) = fetch_metadata::<ProtectedResourceMetadata>(client, candidate).await
        else {
            continue;
        };
        // RFC 9728 §3.3, and the whole point of the RFC 8707 binding below: the
        // document must claim the resource it is protecting. A server naming
        // someone else's resource is asking us to obtain a third-party token and
        // hand it back, so the flow ends here instead of falling through to the
        // origin, which would hide the attempt.
        if let Some(declared) = metadata.resource.as_deref() {
            let declared_url = require_secure(declared, "protected resource")?;
            if !covers_resource(&declared_url, &parsed) {
                return Err(AuthError::Callback(format!(
                    "protected-resource metadata at {candidate} declares resource {declared}, which does not cover {mcp_url}"
                )));
            }
            // First accepted document wins: a later candidate that omits the
            // field must not erase what an earlier one stated.
            resource.get_or_insert_with(|| declared.to_string());
        }
        if !metadata.authorization_servers.is_empty() {
            issuers = metadata.authorization_servers;
            break;
        }
    }

    // No protected-resource document: the spec allows treating the MCP origin as
    // its own authorization server, which is what small self-hosted servers do.
    if issuers.is_empty() {
        issuers.push(format!(
            "{}://{}",
            parsed.scheme(),
            parsed.authority().trim_start_matches('@')
        ));
    }

    // A refused issuer or an unusable document disqualifies that entry alone: a
    // document listing a stale server next to a good one still resolves. The
    // last refusal is kept so the failure names a cause rather than "not found".
    let mut refusal = None;
    for issuer in &issuers {
        let issuer_url = match require_secure(issuer, "authorization server") {
            Ok(url) => url,
            Err(err) => {
                refusal = Some(err);
                continue;
            }
        };
        for candidate in auth_server_urls(&issuer_url) {
            let Some(raw) = fetch_metadata::<RawAuthServerMetadata>(client, &candidate).await
            else {
                continue;
            };
            match validate_auth_server(raw, issuer) {
                Ok(Some(metadata)) => return Ok((metadata, resource)),
                Ok(None) => {}
                Err(err) => refusal = Some(err),
            }
        }
    }

    Err(refusal.unwrap_or_else(|| {
        AuthError::Callback(format!("no OAuth authorization server found for {mcp_url}"))
    }))
}

/// Does the resource identifier a document claims actually cover the MCP
/// endpoint? Same origin, and a path that is a prefix of the endpoint's, so
/// `https://api.example.com/mcp` covers `/mcp/v1` while nothing covers another
/// host. This is the check that keeps the granted token bound to THIS server.
fn covers_resource(declared: &url::Url, mcp: &url::Url) -> bool {
    if declared.scheme() != mcp.scheme()
        || declared.host_str() != mcp.host_str()
        || declared.port_or_known_default() != mcp.port_or_known_default()
    {
        return false;
    }
    let declared_path = declared.path().trim_end_matches('/');
    let mcp_path = mcp.path().trim_end_matches('/');
    declared_path.is_empty()
        || mcp_path == declared_path
        || mcp_path
            .strip_prefix(declared_path)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Turns a raw metadata document into the endpoints a login needs, refusing
/// anything unusable. `Ok(None)` = this document does not describe a usable
/// authorization-code server, keep looking.
fn validate_auth_server(
    raw: RawAuthServerMetadata,
    issuer: &str,
) -> Result<Option<AuthServerMetadata>, AuthError> {
    let (Some(authorization_endpoint), Some(token_endpoint)) =
        (raw.authorization_endpoint, raw.token_endpoint)
    else {
        return Ok(None);
    };
    // PKCE is not optional for a public client. A server that advertises its
    // methods and omits S256 cannot protect the code, so we do not start a flow
    // whose code could be intercepted.
    if !raw.code_challenge_methods_supported.is_empty()
        && !raw
            .code_challenge_methods_supported
            .iter()
            .any(|method| method == "S256")
    {
        return Err(AuthError::Callback(format!(
            "authorization server {issuer} does not support PKCE S256"
        )));
    }
    require_secure(&authorization_endpoint, "authorization endpoint")?;
    require_secure(&token_endpoint, "token endpoint")?;
    if let Some(registration) = &raw.registration_endpoint {
        require_secure(registration, "registration endpoint")?;
    }
    Ok(Some(AuthServerMetadata {
        issuer: raw.issuer,
        authorization_endpoint,
        token_endpoint,
        registration_endpoint: raw.registration_endpoint,
        scopes_supported: raw.scopes_supported,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resource_metadata_hint_is_read_in_both_quoted_and_bare_forms() {
        assert_eq!(
            resource_metadata_hint(
                r#"Bearer realm="mcp", resource_metadata="https://x.example.com/.well-known/oauth-protected-resource""#
            )
            .as_deref(),
            Some("https://x.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(
            resource_metadata_hint("Bearer resource_metadata=https://x.example.com/m, realm=y")
                .as_deref(),
            Some("https://x.example.com/m")
        );
        assert_eq!(resource_metadata_hint("Bearer realm=\"mcp\""), None);
    }

    #[test]
    fn plain_http_endpoints_are_refused_unless_loopback() {
        assert!(require_secure("https://as.example.com/token", "t").is_ok());
        assert!(require_secure("http://127.0.0.1:9000/token", "t").is_ok());
        assert!(require_secure("http://localhost:9000/token", "t").is_ok());
        assert!(require_secure("http://as.example.com/token", "t").is_err());
        // A host that merely LOOKS like loopback resolves wherever its owner wants.
        assert!(require_secure("http://localhost.evil.com/token", "t").is_err());
        assert!(require_secure("ftp://as.example.com/token", "t").is_err());
    }

    #[test]
    fn discovery_urls_try_the_path_aware_form_first() {
        let url = url::Url::parse("https://api.example.com/mcp/v1").unwrap();
        assert_eq!(
            protected_resource_urls(&url),
            vec![
                "https://api.example.com/.well-known/oauth-protected-resource/mcp/v1".to_string(),
                "https://api.example.com/.well-known/oauth-protected-resource".to_string(),
            ]
        );
        let issuer = url::Url::parse("https://as.example.com").unwrap();
        assert_eq!(
            auth_server_urls(&issuer),
            vec![
                "https://as.example.com/.well-known/oauth-authorization-server".to_string(),
                "https://as.example.com/.well-known/openid-configuration".to_string(),
            ]
        );
    }

    /// The check that makes the RFC 8707 binding mean something: a document may
    /// only claim a resource that covers the endpoint it is protecting.
    #[test]
    fn a_declared_resource_must_cover_the_mcp_endpoint() {
        let mcp = url::Url::parse("https://api.example.com/mcp/v1").unwrap();
        let covers = |declared: &str| covers_resource(&url::Url::parse(declared).unwrap(), &mcp);
        assert!(covers("https://api.example.com/mcp/v1"));
        assert!(covers("https://api.example.com/mcp"));
        assert!(covers("https://api.example.com/mcp/"));
        assert!(covers("https://api.example.com"));
        // Another host: this is the confused-deputy case. The user would consent
        // at a real identity provider and the token would come back to whoever
        // asked for it.
        assert!(!covers("https://graph.microsoft.com"));
        // Same host, unrelated path: not the resource being protected.
        assert!(!covers("https://api.example.com/other"));
        // A prefix that is not a path segment boundary is not a prefix.
        assert!(!covers("https://api.example.com/m"));
        // Downgrade and port hop are both a different origin.
        assert!(!covers("http://api.example.com/mcp"));
        assert!(!covers("https://api.example.com:8443/mcp"));
    }

    #[test]
    fn an_authorization_server_without_pkce_s256_is_refused() {
        let raw = RawAuthServerMetadata {
            issuer: None,
            authorization_endpoint: Some("https://as.example.com/authorize".to_string()),
            token_endpoint: Some("https://as.example.com/token".to_string()),
            registration_endpoint: None,
            scopes_supported: Vec::new(),
            code_challenge_methods_supported: vec!["plain".to_string()],
        };
        let err = validate_auth_server(raw, "https://as.example.com").unwrap_err();
        assert!(err.to_string().contains("PKCE S256"), "{err}");
    }

    #[test]
    fn an_incomplete_metadata_document_is_skipped_not_fatal() {
        let raw = RawAuthServerMetadata {
            issuer: None,
            authorization_endpoint: None,
            token_endpoint: Some("https://as.example.com/token".to_string()),
            registration_endpoint: None,
            scopes_supported: Vec::new(),
            code_challenge_methods_supported: Vec::new(),
        };
        assert_eq!(
            validate_auth_server(raw, "https://as.example.com").unwrap(),
            None
        );
    }

    #[test]
    fn a_plain_http_endpoint_in_metadata_is_refused() {
        let raw = RawAuthServerMetadata {
            issuer: None,
            authorization_endpoint: Some("http://as.example.com/authorize".to_string()),
            token_endpoint: Some("https://as.example.com/token".to_string()),
            registration_endpoint: None,
            scopes_supported: Vec::new(),
            code_challenge_methods_supported: Vec::new(),
        };
        assert!(validate_auth_server(raw, "https://as.example.com").is_err());
    }
}
