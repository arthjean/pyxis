//! Device-code flow (RFC 8628 plus Codex specifics), for a host with no browser.

use std::time::Duration;

use super::token::exchange_code;
use super::{CLIENT_ID, DEVICE_REDIRECT_URI, DEVICE_TOKEN_URL, DEVICE_USER_CODE_URL};
use crate::oauth::{AuthError, now_ms};
use crate::{OAuthCredential, Secret};

/// URI displayed to the user in device flow.
pub const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
pub const DEVICE_CODE_TIMEOUT: Duration = Duration::from_secs(900);

/// Information to present to the user for the device flow.
#[derive(Debug, Clone)]
pub struct DeviceAuth {
    pub user_code: Secret,
    pub verification_uri: String,
}

/// Internal poll state (separate from the user-facing display).
#[derive(Debug, Clone)]
pub struct DeviceAuthState {
    device_auth_id: Secret,
    user_code: Secret,
    interval: u64,
}

/// Outcome of a device-code poll.
#[derive(Debug, Clone)]
pub enum PollOutcome {
    Pending,
    SlowDown,
    Done {
        authorization_code: Secret,
        code_verifier: Secret,
    },
}

fn device_error_code(body: &serde_json::Value) -> Option<&str> {
    body.get("errorCode")
        .or_else(|| body.get("error"))
        .and_then(|v| v.as_str())
}

/// Classifies a device-code poll response (RFC 8628 + Codex specifics).
pub fn classify_device_poll(
    status: u16,
    body: &serde_json::Value,
) -> Result<PollOutcome, AuthError> {
    if status == 200 {
        let code = body.get("authorization_code").and_then(|v| v.as_str());
        let verifier = body.get("code_verifier").and_then(|v| v.as_str());
        return match (code, verifier) {
            (Some(c), Some(v)) => Ok(PollOutcome::Done {
                authorization_code: Secret::new(c),
                // in device flow, the code_verifier comes from the SERVER, not locally.
                code_verifier: Secret::new(v),
            }),
            _ => Err(AuthError::TokenResponse(
                "device 200 without authorization_code/code_verifier".to_string(),
            )),
        };
    }
    match device_error_code(body) {
        Some("deviceauth_authorization_pending" | "authorization_pending") => {
            Ok(PollOutcome::Pending)
        }
        Some("slow_down") => Ok(PollOutcome::SlowDown),
        Some("expired_token") => Err(AuthError::DeviceTimeout),
        Some(other) => Err(AuthError::DeviceDenied(other.to_string())),
        None if status == 403 || status == 404 => Ok(PollOutcome::Pending),
        None => Err(AuthError::DeviceDenied(format!("http {status}"))),
    }
}

/// Starts the device flow: returns the state to poll + the info to display.
pub async fn start_device(
    client: &reqwest::Client,
) -> Result<(DeviceAuthState, DeviceAuth), AuthError> {
    let v: serde_json::Value = client
        .post(DEVICE_USER_CODE_URL)
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let field = |name: &str| {
        v.get(name)
            .and_then(|x| x.as_str())
            .map(Secret::new)
            .ok_or_else(|| AuthError::TokenResponse(format!("missing {name}")))
    };
    let device_auth_id = field("device_auth_id")?;
    let user_code = field("user_code")?;
    let interval = v
        .get("interval")
        .and_then(|x| x.as_u64())
        .unwrap_or(5)
        .max(1);

    let display = DeviceAuth {
        user_code: user_code.clone(),
        verification_uri: DEVICE_VERIFICATION_URI.to_string(),
    };
    Ok((
        DeviceAuthState {
            device_auth_id,
            user_code,
            interval,
        },
        display,
    ))
}

/// Polls until authorization, `slow_down`, or timeout (900 s). Final exchange through
/// `DEVICE_REDIRECT_URI`.
pub async fn poll_device(
    client: &reqwest::Client,
    st: &DeviceAuthState,
) -> Result<OAuthCredential, AuthError> {
    let start = tokio::time::Instant::now();
    let mut interval = st.interval;

    loop {
        if start.elapsed() >= DEVICE_CODE_TIMEOUT {
            return Err(AuthError::DeviceTimeout);
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;

        let resp = client
            .post(DEVICE_TOKEN_URL)
            .json(&serde_json::json!({
                "device_auth_id": st.device_auth_id.expose(),
                "user_code": st.user_code.expose(),
            }))
            .send()
            .await?;
        let status = resp.status().as_u16();
        let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);

        match classify_device_poll(status, &body)? {
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval = interval.saturating_add(5),
            PollOutcome::Done {
                authorization_code,
                code_verifier,
            } => {
                return exchange_code(
                    client,
                    &authorization_code,
                    &code_verifier,
                    DEVICE_REDIRECT_URI,
                    now_ms(),
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_poll_classification() {
        let null = serde_json::Value::Null;
        for (status, body) in [
            (403, null.clone()),
            (404, null),
            (
                400,
                serde_json::json!({"errorCode":"deviceauth_authorization_pending"}),
            ),
            (400, serde_json::json!({"error":"authorization_pending"})),
        ] {
            assert!(matches!(
                classify_device_poll(status, &body),
                Ok(PollOutcome::Pending)
            ));
        }
        assert!(matches!(
            classify_device_poll(400, &serde_json::json!({"errorCode":"slow_down"})),
            Ok(PollOutcome::SlowDown)
        ));
        assert!(matches!(
            classify_device_poll(403, &serde_json::json!({"errorCode":"access_denied"})),
            Err(AuthError::DeviceDenied(e)) if e == "access_denied"
        ));
        assert!(matches!(
            classify_device_poll(404, &serde_json::json!({"error":"expired_token"})),
            Err(AuthError::DeviceTimeout)
        ));
        assert!(matches!(
            classify_device_poll(200, &serde_json::json!({"authorization_code":"C"})),
            Err(AuthError::TokenResponse(_))
        ));

        let done = classify_device_poll(
            200,
            &serde_json::json!({"authorization_code":"C","code_verifier":"V"}),
        )
        .unwrap();
        let PollOutcome::Done {
            authorization_code,
            code_verifier,
        } = done
        else {
            unreachable!("a complete 200 is a Done")
        };
        assert_eq!(authorization_code.expose(), "C");
        assert_eq!(code_verifier.expose(), "V");
    }

    #[test]
    fn device_transients_are_redacted_by_their_types() {
        let state = DeviceAuthState {
            device_auth_id: Secret::new("DEVICE_SECRET"),
            user_code: Secret::new("USER_SECRET"),
            interval: 5,
        };
        let rendered = format!(
            "{state:?} {:?} {:?}",
            DeviceAuth {
                user_code: Secret::new("DISPLAY_CODE_SECRET"),
                verification_uri: DEVICE_VERIFICATION_URI.to_string(),
            },
            PollOutcome::Done {
                authorization_code: Secret::new("AUTH_CODE_SECRET"),
                code_verifier: Secret::new("VERIFIER_SECRET"),
            }
        );
        for secret in [
            "DEVICE_SECRET",
            "USER_SECRET",
            "DISPLAY_CODE_SECRET",
            "AUTH_CODE_SECRET",
            "VERIFIER_SECRET",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }
    }
}
