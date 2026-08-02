use super::validation::{fingerprint, invalid, validate_identity, validated_header_value};
use super::{ProviderAuthError, ProviderCredential, ResolvedBedrockAuth};

impl ProviderCredential {
    pub fn resolve_bedrock_api_key(
        &self,
        expected_region: &str,
    ) -> Result<ResolvedBedrockAuth, ProviderAuthError> {
        let Self::BedrockApiKey {
            token,
            region,
            identity,
        } = self
        else {
            return Err(ProviderAuthError::UnsupportedCredential);
        };
        if region != expected_region {
            return Err(invalid(
                "region",
                "does not match the selected Bedrock region",
            ));
        }
        validate_identity("region", region)?;
        validated_header_value("token", token.expose())?;
        Ok(ResolvedBedrockAuth {
            token: token.clone(),
            identity_fingerprint: fingerprint(&[
                "AmazonBedrock",
                region,
                identity.as_deref().unwrap_or_else(|| token.expose()),
            ]),
        })
    }
}
