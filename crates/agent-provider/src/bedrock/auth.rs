use agent_auth::provider::ProviderCredential;
use agent_core::provider::{AuthError, ProviderError};
use aws_credential_types::provider::ProvideCredentials;
use aws_types::region::Region;

use super::{AmazonBedrockConfig, invalid};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BedrockCredentialSource {
    AwsSdkChain,
    BedrockApiKey,
    InjectedSdkClient,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BedrockAccountState {
    pub region: String,
    pub profile: Option<String>,
    pub credential_source: BedrockCredentialSource,
    pub identity_fingerprint: Option<String>,
    pub preferred_models: Vec<String>,
}

pub(super) async fn sdk_chain_client(
    config: &AmazonBedrockConfig,
) -> Result<aws_sdk_bedrockruntime::Client, ProviderError> {
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(Region::new(config.region.clone()));
    if let Some(profile) = config.profile.as_deref() {
        loader = loader.profile_name(profile);
    }
    let shared = loader.load().await;
    let credentials = shared
        .credentials_provider()
        .ok_or(ProviderError::Credential(AuthError::RecoveryUnavailable))?;
    credentials
        .provide_credentials()
        .await
        .map_err(|_| ProviderError::Credential(AuthError::RecoveryUnavailable))?;
    Ok(aws_sdk_bedrockruntime::Client::new(&shared))
}

pub(super) async fn api_key_client(
    config: &AmazonBedrockConfig,
    credential: &ProviderCredential,
) -> Result<(aws_sdk_bedrockruntime::Client, String), ProviderError> {
    let resolved = credential
        .resolve_bedrock_api_key(&config.region)
        .map_err(|error| invalid("auth", error.to_string()))?;
    let shared = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(Region::new(config.region.clone()))
        .load()
        .await;
    let sdk_config = aws_sdk_bedrockruntime::config::Builder::from(&shared)
        .bearer_token(aws_sdk_bedrockruntime::config::Token::new(
            resolved.token().expose().to_string(),
            None,
        ))
        .auth_scheme_preference([
            aws_smithy_runtime_api::client::auth::http::HTTP_BEARER_AUTH_SCHEME_ID,
        ])
        .build();
    Ok((
        aws_sdk_bedrockruntime::Client::from_conf(sdk_config),
        resolved.identity_fingerprint,
    ))
}
