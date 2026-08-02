//! Opt-in live proof for the ChatGPT subscription Responses WebSocket.
//!
//! Disabled unless `PYXIS_WEBSOCKET_PROBE=1` is present. The report contains no
//! credential, account identifier, response identifier, or response payload.

use agent_auth::{Credential, ProviderId, store};
use agent_core::message::Message;
use agent_core::provider::{CanonicalRequest, TURN_ID_METADATA_KEY};
use agent_provider::{
    DEFAULT_MODEL, KEYRING_ACCOUNT, OpenAiChatGptProvider, WebSocketProbeAuthorization,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("PYXIS_WEBSOCKET_PROBE").as_deref() != Ok("1") {
        return Err("live probe disabled; set PYXIS_WEBSOCKET_PROBE=1 to authorize it".into());
    }

    let credential = match store::load(KEYRING_ACCOUNT)? {
        Some(Credential::Oauth(credential)) if credential.provider == ProviderId::OpenAiChatGpt => {
            credential
        }
        _ => return Err("a valid ChatGPT credential is required in the Pyxis keyring".into()),
    };
    let model = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let provider = OpenAiChatGptProvider::from_credential(credential)?;
    let request = CanonicalRequest {
        model,
        system: Some("Return exactly: websocket probe ok".into()),
        messages: vec![Message::user("Run the transport probe.")],
        max_output_tokens: 64,
        client_metadata: std::collections::BTreeMap::from([
            ("session_id".into(), "pyxis-websocket-probe".into()),
            ("thread_id".into(), "pyxis-websocket-probe".into()),
            (TURN_ID_METADATA_KEY.into(), "pyxis-websocket-probe".into()),
        ]),
        ..CanonicalRequest::default()
    };
    let report = provider
        .probe_websocket(
            WebSocketProbeAuthorization::explicitly_authorized(),
            request,
        )
        .await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
