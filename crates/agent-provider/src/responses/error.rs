use agent_core::provider::{AuthError, ErrorClass, ProviderError, ProviderErrorCategory};

use crate::chatgpt_error::{is_terminal_rate_limit, should_retry_without_reasoning_replay};

pub(crate) fn classify_error(error: &ProviderError) -> ErrorClass {
    match error {
        ProviderError::Credential(error) => ErrorClass::Auth(*error),
        ProviderError::Http {
            status, message, ..
        } => classify_http(*status, message),
        ProviderError::Api {
            category,
            status,
            message,
            ..
        } => match category {
            ProviderErrorCategory::Authentication => ErrorClass::Auth(AuthError::Expired),
            ProviderErrorCategory::PermissionDenied => ErrorClass::Auth(AuthError::Invalid),
            ProviderErrorCategory::RateLimited => ErrorClass::RateLimited,
            ProviderErrorCategory::Overloaded => ErrorClass::Overloaded(status.unwrap_or(529)),
            ProviderErrorCategory::Failed if status.is_some_and(|status| status >= 500) => {
                ErrorClass::Retryable
            }
            ProviderErrorCategory::Incomplete => ErrorClass::Retryable,
            ProviderErrorCategory::InvalidRequest
                if status == &Some(400) && should_retry_without_reasoning_replay(400, message) =>
            {
                ErrorClass::ReasoningReplayRejected
            }
            _ => ErrorClass::InvalidRequest,
        },
        ProviderError::Transport(_) | ProviderError::Decode(_) | ProviderError::Stream(_) => {
            ErrorClass::Retryable
        }
        ProviderError::UnsupportedTool { .. }
        | ProviderError::UnsupportedCapability { .. }
        | ProviderError::ContextLengthExceeded => ErrorClass::InvalidRequest,
    }
}

fn classify_http(status: u16, message: &str) -> ErrorClass {
    match status {
        401 => ErrorClass::Auth(AuthError::Expired),
        403 => ErrorClass::Auth(AuthError::Invalid),
        400 if should_retry_without_reasoning_replay(status, message) => {
            ErrorClass::ReasoningReplayRejected
        }
        429 if is_terminal_rate_limit(message) => ErrorClass::InvalidRequest,
        429 => ErrorClass::RateLimited,
        529 => ErrorClass::Overloaded(529),
        status if status >= 500 => ErrorClass::Retryable,
        _ => ErrorClass::InvalidRequest,
    }
}

pub(crate) fn reasoning_effort_for_request(effort: &str) -> &str {
    if effort.eq_ignore_ascii_case("ultra") {
        "max"
    } else {
        effort
    }
}
