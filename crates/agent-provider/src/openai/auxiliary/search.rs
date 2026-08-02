use agent_core::auxiliary::search::{
    SearchCommands, SearchInput, SearchPagination, SearchRequest, SearchResponse, SearchSettings,
};
use agent_core::auxiliary::{AuxiliaryError, AuxiliaryOperation};
use agent_core::provider::ResponseItem;
use serde::Serialize;
use serde_json::Value;

use super::ConfiguredOpenAiProvider;
use super::json::{decode, optional_string, sanitized_metadata};
use super::validation::nonempty;

#[derive(Serialize)]
struct SearchWireRequest<'a> {
    id: &'a str,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<SearchWireInput<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commands: Option<&'a SearchCommands>,
    #[serde(skip_serializing_if = "Option::is_none")]
    settings: Option<&'a SearchSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum SearchWireInput<'a> {
    Text(&'a str),
    Items(Vec<&'a Value>),
}

pub(super) async fn execute(
    provider: &ConfiguredOpenAiProvider,
    request: &SearchRequest,
) -> Result<SearchResponse, AuxiliaryError> {
    let operation = AuxiliaryOperation::Search;
    super::ensure_supported(provider, operation)?;
    nonempty(operation, "id", &request.id, 256)?;
    nonempty(operation, "model", &request.model, 256)?;
    if request.input.is_none() && request.commands.is_none() {
        return Err(AuxiliaryError::invalid(
            operation,
            "input",
            "input or commands are required",
        ));
    }
    let input = request.input.as_ref().map(|input| match input {
        SearchInput::Text(text) => SearchWireInput::Text(text),
        SearchInput::Items(items) => {
            SearchWireInput::Items(items.iter().map(|item| item.payload().payload()).collect())
        }
    });
    let wire = SearchWireRequest {
        id: &request.id,
        model: &request.model,
        reasoning: request.reasoning.as_ref(),
        input,
        commands: request.commands.as_ref(),
        settings: request.settings.as_ref(),
        max_output_tokens: request.max_output_tokens,
    };
    let response = provider
        .auxiliary_json(operation, "alpha/search", &wire)
        .await?;
    decode_response(operation, &response.body)
}

fn decode_response(
    operation: AuxiliaryOperation,
    body: &[u8],
) -> Result<SearchResponse, AuxiliaryError> {
    let value: Value = decode(operation, body)?;
    let output = value
        .get("output")
        .and_then(Value::as_str)
        .ok_or_else(|| AuxiliaryError::decode(operation, "missing output string"))?
        .to_string();
    let encrypted_output =
        optional_string(operation, value.get("encrypted_output"), 16 * 1024 * 1024)?;
    let results = optional_array(value.get("results"), operation, "results")?;
    let items = match value.get("items") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|item| {
                    ResponseItem::from_wire(item)
                        .map_err(|_| AuxiliaryError::decode(operation, "invalid response item"))
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err(AuxiliaryError::decode(operation, "invalid items array")),
    };
    let pagination = match value.get("pagination") {
        Some(pagination) => serde_json::from_value(pagination.clone())
            .map_err(|_| AuxiliaryError::decode(operation, "invalid pagination"))?,
        None => SearchPagination {
            next_page: optional_string(operation, value.get("next_page"), 4096)?,
            has_more: value.get("has_more").and_then(Value::as_bool),
        },
    };
    let metadata = sanitized_metadata(
        "auxiliary.search.response",
        value,
        &["encrypted_output", "output", "results", "items"],
    );
    Ok(SearchResponse {
        encrypted_output,
        output,
        results,
        items,
        pagination,
        metadata,
    })
}

fn optional_array(
    value: Option<&Value>,
    operation: AuxiliaryOperation,
    field: &'static str,
) -> Result<Option<Vec<Value>>, AuxiliaryError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(values)) => Ok(Some(values.clone())),
        Some(_) => Err(AuxiliaryError::decode(
            operation,
            format!("invalid {field} array"),
        )),
    }
}
