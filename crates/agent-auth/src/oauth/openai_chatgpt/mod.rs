//! ChatGPT subscription auth (ADR-10). Reuses the OAuth client of the **official
//! OSS Codex CLI**: PKCE S256 flow on `auth.openai.com` (browser + device-code),
//! JWT decoding for `chatgpt_account_id`, rotating refresh tokens.
//!
//! Constants checked verbatim against the Pi repo (`packages/ai/src/utils/oauth/
//! openai-codex.ts` + `providers/openai-codex-responses.ts`, 45/45 confirmed).
//! Details & sources: `docs/openai-subscription-auth.md`.
//!
//! ToS grey area: it impersonates Codex (shared client_id), **revocable
//! unilaterally by OpenAI** (see ADR-7 R1, ADR-10). A "fragile" credential,
//! never on the critical path: it is a dogfooding convenience behind BYOK.

use std::sync::OnceLock;
use std::time::Duration;

use super::callback::Callback;
use super::pkce::Pkce;
use super::{AuthError, now_ms, random_state};
use crate::provider::ProviderRequestAuth;
use crate::{OAuthCredential, ProviderId, Secret};

mod device;
mod token;

pub use device::{
    DEVICE_CODE_TIMEOUT, DEVICE_VERIFICATION_URI, DeviceAuth, DeviceAuthState, PollOutcome,
    classify_device_poll, poll_device, start_device,
};
pub use token::{exchange_code, extract_account_id, refresh};

// ───────────────── Auth constants (auth.openai.com), verbatim from Pi ─────────────────

/// `client_id` of the OSS Codex CLI (`openai-codex.ts:31`).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
/// `redirect_uri` of the code -> token exchange in **device** flow (different from browser).
pub const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
pub const SCOPE: &str = "openid profile email offline_access";
pub const CALLBACK_PORT: u16 = 1455;
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(900);
/// Namespace of the custom claim where `chatgpt_account_id` lives (`openai-codex.ts:44`).
pub const JWT_CLAIM_NAMESPACE: &str = "https://api.openai.com/auth";
/// Hardcoded per client (Pi uses `"pi"`). The ChatGPT backend **may** validate
/// the `originator` against a known list: to be tested on the first run (ADR-10).
pub const ORIGINATOR: &str = "pyxis";

/// `originator` fallback when the backend rejects `pyxis` (US-021, unhappy path):
/// borrow the identity of the official OSS Codex CLI, already on the backend
/// allow-list. Set `PYXIS_ORIGINATOR` to this value to switch without recompiling.
pub const ORIGINATOR_FALLBACK: &str = "codex_cli_rs";

/// Effective `originator` sent on the INFERENCE request (US-021).
///
/// Resolved once per process: an env read on every request is both a cost on
/// the hot path and, since the 2024 edition, a race against any `set_var` in
/// another thread. Nothing wants this value to change mid-session anyway.
/// Does NOT affect the OAuth flow: `build_authorize_url` keeps `ORIGINATOR`
/// (changing the auth would break the flow validated live, out of scope).
pub fn originator() -> &'static str {
    static EFFECTIVE: OnceLock<String> = OnceLock::new();
    EFFECTIVE
        .get_or_init(|| env_override("PYXIS_ORIGINATOR").unwrap_or_else(|| ORIGINATOR.to_string()))
        .as_str()
}

// ───────────────── Inference constants (ChatGPT backend, Responses API) ─────────────────

pub const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const RESPONSES_PATH: &str = "/responses";
pub const MODELS_PATH: &str = "/models";
pub const OPENAI_BETA_SSE: &str = "responses=experimental";

/// Effective build version sent on `/models`. Release builds can inject the
/// compatibility version through `PYXIS_CODEX_CLIENT_VERSION` at compile time;
/// local builds fall back to the package version.
pub const fn build_codex_client_version() -> &'static str {
    match option_env!("PYXIS_CODEX_CLIENT_VERSION") {
        Some(version) => version,
        None => env!("CARGO_PKG_VERSION"),
    }
}

/// The build version, with a runtime override for diagnostics and compatibility
/// probes. Resolved once, for the same reason as [`originator`].
pub fn codex_client_version() -> &'static str {
    static EFFECTIVE: OnceLock<String> = OnceLock::new();
    EFFECTIVE
        .get_or_init(|| {
            env_override("PYXIS_CODEX_CLIENT_VERSION")
                .unwrap_or_else(|| build_codex_client_version().to_string())
        })
        .as_str()
}

fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// ──────────────────────────── Authorization URL ────────────────────────────

/// Builds the authorization URL (browser flow). Includes the
/// non-standard parameters required by the Codex backend (`id_token_add_organizations`,
/// `codex_cli_simplified_flow`).
pub fn build_authorize_url(challenge: &Secret, state: &str) -> Result<String, AuthError> {
    let mut url = url::Url::parse(AUTHORIZE_URL)
        .map_err(|e| AuthError::discovery(format!("authorize endpoint: {e}")))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge.expose())
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", ORIGINATOR);
    Ok(url.to_string())
}

// ──────────────────────────── Inference requests ────────────────────────────

/// SSE inference headers for a ChatGPT subscription credential. The
/// `chatgpt-account-id` (derived from the JWT) is required to route to the account.
pub fn responses_request(cred: &OAuthCredential) -> Result<ProviderRequestAuth, AuthError> {
    let mut headers = auth_headers(cred)?;
    headers.push(("OpenAI-Beta".to_string(), Secret::new(OPENAI_BETA_SSE)));
    headers.push(("accept".to_string(), Secret::new("text/event-stream")));
    headers.push(("content-type".to_string(), Secret::new("application/json")));
    Ok(ProviderRequestAuth {
        url: format!("{CHATGPT_BASE_URL}{RESPONSES_PATH}"),
        headers,
    })
}

/// Model catalog discovery request (`GET /models`). The backend
/// returns the models accessible TO THE connected ACCOUNT (`available_in_plans` field
/// already applied), filtered by the current build's `client_version`.
pub fn models_request(cred: &OAuthCredential) -> Result<ProviderRequestAuth, AuthError> {
    let mut headers = auth_headers(cred)?;
    headers.push(("accept".to_string(), Secret::new("application/json")));
    let mut url = url::Url::parse(&format!("{CHATGPT_BASE_URL}{MODELS_PATH}"))
        .map_err(|e| AuthError::discovery(format!("models endpoint: {e}")))?;
    url.query_pairs_mut()
        .append_pair("client_version", codex_client_version());
    Ok(ProviderRequestAuth {
        url: url.to_string(),
        headers,
    })
}

/// Identification headers common to every Codex backend request.
fn auth_headers(cred: &OAuthCredential) -> Result<Vec<(String, Secret)>, AuthError> {
    if cred.provider != ProviderId::OpenAiChatGpt {
        return Err(AuthError::WrongProvider(cred.provider));
    }
    let account_id = cred
        .account_id
        .as_deref()
        .ok_or(AuthError::MissingAccountId)?;
    Ok(vec![
        (
            "Authorization".to_string(),
            Secret::new(format!("Bearer {}", cred.access.expose())),
        ),
        ("chatgpt-account-id".to_string(), Secret::new(account_id)),
        ("originator".to_string(), Secret::new(originator())),
    ])
}

// ──────────────────────────── Browser flow ────────────────────────────

/// Interactive login: opens the browser, waits for the callback on `127.0.0.1:1455`,
/// exchanges the code. Opening the browser is best-effort, and this crate does not
/// decide what a failure looks like: the caller receives the URL through
/// `on_auth_url` and prints it itself. That is FR-15 (US-020): only a binary writes
/// on a process output, because only a binary knows whether a TUI owns the
/// terminal.
pub async fn login_browser_with_notice<F>(
    client: &reqwest::Client,
    on_auth_url: F,
) -> Result<OAuthCredential, AuthError>
where
    F: FnOnce(&str, bool),
{
    let pkce = Pkce::generate();
    let state = random_state();
    let url = build_authorize_url(&pkce.challenge, &state)?;

    // bind BEFORE opening the browser (otherwise a race on the callback)
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await?;
    let callback = Callback {
        path: CALLBACK_PATH,
        headline: "Pyxis connected",
        expected_state: &state,
    };

    let opened = open::that(&url).is_ok();
    on_auth_url(&url, opened);

    let code = tokio::time::timeout(CALLBACK_TIMEOUT, callback.accept(&listener))
        .await
        .map_err(|_| AuthError::timeout("OAuth callback"))??;
    exchange_code(client, &code, &pkce.verifier, REDIRECT_URI, now_ms()).await
}

// ──────────────────────────────── Tests ────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_contains_required_params() {
        let url = build_authorize_url(&Secret::new("CHAL"), "STATE123").unwrap();
        for needle in [
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            "code_challenge=CHAL",
            "code_challenge_method=S256",
            "state=STATE123",
            "id_token_add_organizations=true",
            "codex_cli_simplified_flow=true",
            "originator=pyxis",
            "scope=openid",
        ] {
            assert!(url.contains(needle), "param absent: {needle}\n{url}");
        }
        // encoded redirect_uri
        assert!(url.contains("redirect_uri=http"));
    }

    #[test]
    fn default_catalog_client_version_is_derived_from_the_build() {
        assert_eq!(
            build_codex_client_version(),
            option_env!("PYXIS_CODEX_CLIENT_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
        );
    }

    // US-021 AC2: `pyxis` by default; `PYXIS_ORIGINATOR=codex_cli_rs` borrows the
    // allow-listed identity of the official CLI when the backend rejects ours.
    #[test]
    fn the_default_originator_is_pyxis_and_the_fallback_is_the_codex_cli() {
        assert_eq!(originator(), "pyxis");
        assert_eq!(ORIGINATOR_FALLBACK, "codex_cli_rs");
    }

    fn chatgpt_credential(account_id: Option<&str>) -> OAuthCredential {
        OAuthCredential {
            provider: ProviderId::OpenAiChatGpt,
            access: Secret::new("AT"),
            refresh: Secret::new("RT"),
            expires_at: 0,
            account_id: account_id.map(str::to_string),
        }
    }

    #[test]
    fn responses_request_has_proprietary_headers() {
        let spec = responses_request(&chatgpt_credential(Some("acct_7"))).unwrap();
        assert_eq!(spec.url, "https://chatgpt.com/backend-api/codex/responses");
        let headers: std::collections::HashMap<_, _> = spec
            .header_pairs()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        assert_eq!(headers["Authorization"], "Bearer AT");
        assert_eq!(headers["chatgpt-account-id"], "acct_7");
        assert_eq!(headers["originator"], "pyxis");
        assert_eq!(headers["OpenAI-Beta"], "responses=experimental");
    }

    #[test]
    fn responses_request_rejects_wrong_provider() {
        let cred = OAuthCredential {
            provider: ProviderId::Anthropic,
            ..chatgpt_credential(Some("acct_7"))
        };
        assert!(matches!(
            responses_request(&cred),
            Err(AuthError::WrongProvider(ProviderId::Anthropic))
        ));
    }

    #[test]
    fn responses_request_without_account_id_errors() {
        assert!(matches!(
            responses_request(&chatgpt_credential(None)),
            Err(AuthError::MissingAccountId)
        ));
    }

    #[test]
    fn a_request_spec_never_renders_its_secrets() {
        let cred = OAuthCredential {
            access: Secret::new("AT_SECRET"),
            ..chatgpt_credential(Some("acct_7"))
        };
        let rendered = format!("{:?}", responses_request(&cred).unwrap());
        assert!(!rendered.contains("AT_SECRET"));
        assert!(!rendered.contains("acct_7"));
        assert!(rendered.contains("Secret(***)"));
    }
}
