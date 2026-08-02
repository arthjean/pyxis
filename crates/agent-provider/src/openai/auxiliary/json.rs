use agent_core::auxiliary::{AuxiliaryError, AuxiliaryOperation};
use agent_core::provider::ProviderExtension;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub(super) fn decode<T: DeserializeOwned>(
    operation: AuxiliaryOperation,
    body: &[u8],
) -> Result<T, AuxiliaryError> {
    serde_json::from_slice(body)
        .map_err(|_| AuxiliaryError::decode(operation, "invalid JSON response"))
}

pub(super) fn optional_string(
    operation: AuxiliaryOperation,
    value: Option<&Value>,
    max_bytes: usize,
) -> Result<Option<String>, AuxiliaryError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.len() <= max_bytes => Ok(Some(value.clone())),
        Some(_) => Err(AuxiliaryError::decode(
            operation,
            "invalid or oversized string field",
        )),
    }
}

pub(super) fn optional_typed<T: DeserializeOwned>(
    value: &Value,
    field: &'static str,
    operation: AuxiliaryOperation,
) -> Result<Option<T>, AuxiliaryError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|_| AuxiliaryError::decode(operation, format!("invalid {field}"))),
    }
}

pub(super) fn sanitized_metadata(
    event_type: &str,
    mut value: Value,
    removed_fields: &[&str],
) -> ProviderExtension {
    let mut removed = false;
    if let Some(object) = value.as_object_mut() {
        for field in removed_fields {
            removed |= object.remove(*field).is_some();
        }
    }
    ProviderExtension::from_value_with_redaction(event_type, value, removed)
}
