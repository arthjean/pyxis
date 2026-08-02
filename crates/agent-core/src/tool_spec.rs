//! Provider-neutral tool algebra and its local validation boundary.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Grammar syntax accepted by a freeform tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrammarSyntax {
    Lark,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolGrammar {
    pub syntax: GrammarSyntax,
    pub definition: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchContextSize {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WebSearchFilters {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_domains: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WebSearchLocation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

/// How a tool receives its input. A function takes JSON validated by a schema;
/// a freeform tool takes text with an optional grammar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolKind {
    Function {
        input_schema: serde_json::Value,
        /// Old serialized specs predate this field and were always strict.
        #[serde(default = "default_true", skip_serializing_if = "is_true")]
        strict: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        defer_loading: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_schema: Option<serde_json::Value>,
    },
    Freeform {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grammar: Option<ToolGrammar>,
    },
    Namespace {
        tools: Vec<ToolSpec>,
    },
    ToolSearch {
        execution: String,
        parameters: serde_json::Value,
    },
    WebSearch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        external_web_access: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        indexed_web_access: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filters: Option<WebSearchFilters>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<WebSearchLocation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_size: Option<WebSearchContextSize>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content_types: Vec<String>,
    },
}

/// Tool definition exposed to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    #[serde(flatten)]
    pub kind: ToolKind,
}

impl ToolSpec {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
    ) -> Self {
        Self::function_with_options(name, description, input_schema, true, false, None)
    }

    pub fn function_with_options(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
        strict: bool,
        defer_loading: bool,
        output_schema: Option<serde_json::Value>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            kind: ToolKind::Function {
                input_schema,
                strict,
                defer_loading,
                output_schema,
            },
        }
    }

    pub fn freeform(
        name: impl Into<String>,
        description: impl Into<String>,
        grammar: Option<ToolGrammar>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            kind: ToolKind::Freeform { grammar },
        }
    }

    pub fn namespace(
        name: impl Into<String>,
        description: impl Into<String>,
        tools: Vec<ToolSpec>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            kind: ToolKind::Namespace { tools },
        }
    }

    pub fn tool_search(
        description: impl Into<String>,
        execution: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: "tool_search".into(),
            description: description.into(),
            kind: ToolKind::ToolSearch {
                execution: execution.into(),
                parameters,
            },
        }
    }

    pub fn web_search(
        filters: Option<WebSearchFilters>,
        location: Option<WebSearchLocation>,
        context_size: Option<WebSearchContextSize>,
    ) -> Self {
        Self {
            name: "web_search".into(),
            description: String::new(),
            kind: ToolKind::WebSearch {
                external_web_access: None,
                indexed_web_access: None,
                filters,
                location,
                context_size,
                content_types: Vec::new(),
            },
        }
    }

    /// A freeform tool has no schema. Callers must not fabricate one to fit a
    /// function-shaped wire.
    pub fn input_schema(&self) -> Option<&serde_json::Value> {
        match &self.kind {
            ToolKind::Function { input_schema, .. } => Some(input_schema),
            ToolKind::Freeform { .. }
            | ToolKind::Namespace { .. }
            | ToolKind::ToolSearch { .. }
            | ToolKind::WebSearch { .. } => None,
        }
    }

    pub fn is_freeform(&self) -> bool {
        matches!(self.kind, ToolKind::Freeform { .. })
    }

    pub fn is_deferred(&self) -> bool {
        match &self.kind {
            ToolKind::Function { defer_loading, .. } => *defer_loading,
            ToolKind::Namespace { tools } => tools.iter().any(Self::is_deferred),
            _ => false,
        }
    }

    pub fn validate(&self) -> Result<(), ToolSpecValidationError> {
        validate_name(&self.name)?;
        match &self.kind {
            ToolKind::Function {
                input_schema,
                strict,
                output_schema,
                ..
            } => {
                validate_object_schema(&self.name, input_schema, SchemaRole::Input)?;
                if *strict {
                    validate_strict_schema_object(&self.name, input_schema, SchemaRole::Input)?;
                }
                if let Some(output_schema) = output_schema {
                    validate_object_schema(&self.name, output_schema, SchemaRole::Output)?;
                    if *strict {
                        validate_strict_schema_object(
                            &self.name,
                            output_schema,
                            SchemaRole::Output,
                        )?;
                    }
                }
            }
            ToolKind::Freeform { grammar } => {
                if grammar
                    .as_ref()
                    .is_some_and(|grammar| grammar.definition.trim().is_empty())
                {
                    return Err(ToolSpecValidationError::EmptyGrammarDefinition {
                        tool: self.name.clone(),
                    });
                }
            }
            ToolKind::Namespace { tools } => self.validate_namespace(tools)?,
            ToolKind::ToolSearch {
                execution,
                parameters,
            } => {
                if execution.trim().is_empty() {
                    return Err(ToolSpecValidationError::EmptyToolSearchExecution);
                }
                if !parameters.is_object() {
                    return Err(ToolSpecValidationError::ToolSearchParametersMustBeObject);
                }
            }
            ToolKind::WebSearch {
                filters,
                location,
                content_types,
                ..
            } => validate_web_search(filters.as_ref(), location.as_ref(), content_types)?,
        }
        Ok(())
    }

    fn validate_namespace(&self, tools: &[ToolSpec]) -> Result<(), ToolSpecValidationError> {
        if tools.is_empty() {
            return Err(ToolSpecValidationError::EmptyNamespace {
                namespace: self.name.clone(),
            });
        }
        let mut names = HashSet::new();
        for tool in tools {
            tool.validate()?;
            if !matches!(tool.kind, ToolKind::Function { .. }) {
                return Err(ToolSpecValidationError::InvalidNamespaceMember {
                    namespace: self.name.clone(),
                    tool: tool.name.clone(),
                });
            }
            if !names.insert(tool.name.as_str()) {
                return Err(ToolSpecValidationError::DuplicateNamespaceMember {
                    namespace: self.name.clone(),
                    tool: tool.name.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum SchemaRole {
    Input,
    Output,
}

fn validate_name(name: &str) -> Result<(), ToolSpecValidationError> {
    if name.trim().is_empty() {
        return Err(ToolSpecValidationError::EmptyName);
    }
    if name.len() > 64
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(ToolSpecValidationError::InvalidName {
            tool: name.to_string(),
        });
    }
    Ok(())
}

fn validate_web_search(
    filters: Option<&WebSearchFilters>,
    location: Option<&WebSearchLocation>,
    content_types: &[String],
) -> Result<(), ToolSpecValidationError> {
    let invalid_filters = filters.is_some_and(|filters| {
        filters.allowed_domains.len() > 100
            || filters.allowed_domains.iter().any(|domain| {
                domain.trim().is_empty()
                    || domain.len() > 253
                    || domain.chars().any(char::is_control)
            })
    });
    let invalid_location = location.is_some_and(|location| {
        [
            location.country.as_deref(),
            location.region.as_deref(),
            location.city.as_deref(),
            location.timezone.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|value| value.is_empty() || value.len() > 128 || value.chars().any(char::is_control))
    });
    let invalid_content_types = content_types.len() > 16
        || content_types
            .iter()
            .any(|kind| kind.is_empty() || kind.len() > 64 || kind.chars().any(char::is_control));
    if invalid_filters || invalid_location || invalid_content_types {
        Err(ToolSpecValidationError::InvalidWebSearchConfig)
    } else {
        Ok(())
    }
}

fn validate_object_schema(
    tool: &str,
    schema: &serde_json::Value,
    role: SchemaRole,
) -> Result<(), ToolSpecValidationError> {
    if schema.as_object().is_some_and(schema_has_object_type) {
        Ok(())
    } else {
        Err(role.invalid_object(tool))
    }
}

fn schema_has_object_type(schema: &serde_json::Map<String, serde_json::Value>) -> bool {
    match schema.get("type") {
        Some(serde_json::Value::String(kind)) => kind == "object",
        Some(serde_json::Value::Array(kinds)) => {
            kinds.iter().any(|kind| kind.as_str() == Some("object"))
        }
        _ => false,
    }
}

fn validate_strict_schema_object(
    tool: &str,
    schema: &serde_json::Value,
    role: SchemaRole,
) -> Result<(), ToolSpecValidationError> {
    let Some(object) = schema.as_object() else {
        return Ok(());
    };

    if schema_has_object_type(object) {
        if object.get("additionalProperties") != Some(&serde_json::Value::Bool(false)) {
            return Err(role.additional_properties(tool));
        }
        let property_names: HashSet<String> = match object.get("properties") {
            None => HashSet::new(),
            Some(serde_json::Value::Object(properties)) => properties.keys().cloned().collect(),
            Some(_) => return Err(role.properties(tool)),
        };
        if required_names(tool, object, role)? != property_names {
            return Err(role.required(tool));
        }
    }

    if let Some(serde_json::Value::Object(properties)) = object.get("properties") {
        for schema in properties.values() {
            validate_strict_schema_object(tool, schema, role)?;
        }
    }
    for key in ["items", "additionalItems", "contains"] {
        if let Some(schema) = object.get(key) {
            validate_strict_schema_object(tool, schema, role)?;
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(serde_json::Value::Array(items)) = object.get(key) {
            for schema in items {
                validate_strict_schema_object(tool, schema, role)?;
            }
        }
    }
    for key in ["$defs", "definitions"] {
        if let Some(serde_json::Value::Object(definitions)) = object.get(key) {
            for schema in definitions.values() {
                validate_strict_schema_object(tool, schema, role)?;
            }
        }
    }
    Ok(())
}

fn required_names(
    tool: &str,
    schema: &serde_json::Map<String, serde_json::Value>,
    role: SchemaRole,
) -> Result<HashSet<String>, ToolSpecValidationError> {
    match schema.get("required") {
        None => Ok(HashSet::new()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| role.required_array(tool))
            })
            .collect(),
        Some(_) => Err(role.required_array(tool)),
    }
}

impl SchemaRole {
    fn invalid_object(self, tool: &str) -> ToolSpecValidationError {
        match self {
            Self::Input => ToolSpecValidationError::SchemaMustBeObject {
                tool: tool.to_string(),
            },
            Self::Output => ToolSpecValidationError::OutputSchemaMustBeObject {
                tool: tool.to_string(),
            },
        }
    }

    fn additional_properties(self, tool: &str) -> ToolSpecValidationError {
        match self {
            Self::Input => ToolSpecValidationError::SchemaMustDenyAdditionalProperties {
                tool: tool.to_string(),
            },
            Self::Output => ToolSpecValidationError::OutputSchemaMustDenyAdditionalProperties {
                tool: tool.to_string(),
            },
        }
    }

    fn properties(self, tool: &str) -> ToolSpecValidationError {
        match self {
            Self::Input => ToolSpecValidationError::SchemaPropertiesMustBeObject {
                tool: tool.to_string(),
            },
            Self::Output => ToolSpecValidationError::OutputSchemaPropertiesMustBeObject {
                tool: tool.to_string(),
            },
        }
    }

    fn required_array(self, tool: &str) -> ToolSpecValidationError {
        match self {
            Self::Input => ToolSpecValidationError::SchemaRequiredMustBeStringArray {
                tool: tool.to_string(),
            },
            Self::Output => ToolSpecValidationError::OutputSchemaRequiredMustBeStringArray {
                tool: tool.to_string(),
            },
        }
    }

    fn required(self, tool: &str) -> ToolSpecValidationError {
        match self {
            Self::Input => ToolSpecValidationError::RequiredMustMatchProperties {
                tool: tool.to_string(),
            },
            Self::Output => ToolSpecValidationError::OutputRequiredMustMatchProperties {
                tool: tool.to_string(),
            },
        }
    }
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolSpecValidationError {
    #[error("tool name is empty")]
    EmptyName,
    #[error("tool {tool} name must be <=64 chars and use only ASCII letters, digits, _ or -")]
    InvalidName { tool: String },
    #[error("tool {tool} input_schema must be a JSON schema object")]
    SchemaMustBeObject { tool: String },
    #[error("tool {tool} output_schema must be a JSON schema object")]
    OutputSchemaMustBeObject { tool: String },
    #[error("tool {tool} input_schema must set additionalProperties=false")]
    SchemaMustDenyAdditionalProperties { tool: String },
    #[error("tool {tool} output_schema must set additionalProperties=false")]
    OutputSchemaMustDenyAdditionalProperties { tool: String },
    #[error("tool {tool} input_schema properties must be an object")]
    SchemaPropertiesMustBeObject { tool: String },
    #[error("tool {tool} output_schema properties must be an object")]
    OutputSchemaPropertiesMustBeObject { tool: String },
    #[error("tool {tool} input_schema required must be an array of strings")]
    SchemaRequiredMustBeStringArray { tool: String },
    #[error("tool {tool} output_schema required must be an array of strings")]
    OutputSchemaRequiredMustBeStringArray { tool: String },
    #[error("tool {tool} input_schema required fields must include every property")]
    RequiredMustMatchProperties { tool: String },
    #[error("tool {tool} output_schema required fields must include every property")]
    OutputRequiredMustMatchProperties { tool: String },
    #[error("tool {tool} declares a grammar with an empty definition")]
    EmptyGrammarDefinition { tool: String },
    #[error("namespace {namespace} must contain at least one tool")]
    EmptyNamespace { namespace: String },
    #[error("namespace {namespace} contains unsupported member {tool}")]
    InvalidNamespaceMember { namespace: String, tool: String },
    #[error("namespace {namespace} contains duplicate member {tool}")]
    DuplicateNamespaceMember { namespace: String, tool: String },
    #[error("tool_search execution is empty")]
    EmptyToolSearchExecution,
    #[error("tool_search parameters must be a JSON schema object")]
    ToolSearchParametersMustBeObject,
    #[error("web_search configuration is invalid or exceeds its bounds")]
    InvalidWebSearchConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strict_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        })
    }

    #[test]
    fn strict_schema_requires_all_properties() {
        let spec = ToolSpec::function(
            "read",
            "lit",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": ["integer", "null"]}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        );
        assert!(matches!(
            spec.validate(),
            Err(ToolSpecValidationError::RequiredMustMatchProperties { tool }) if tool == "read"
        ));
    }

    #[test]
    fn output_schema_errors_name_the_output_boundary() {
        let spec = ToolSpec::function_with_options(
            "read",
            "lit",
            strict_schema(),
            true,
            false,
            Some(serde_json::json!({
                "type": "object",
                "properties": {"ok": {"type": "boolean"}},
                "required": [],
                "additionalProperties": false
            })),
        );
        assert!(matches!(
            spec.validate(),
            Err(ToolSpecValidationError::OutputRequiredMustMatchProperties { .. })
        ));
    }

    #[test]
    fn freeform_and_namespace_specs_round_trip() {
        let freeform = ToolSpec::freeform(
            "exec",
            "run javascript",
            Some(ToolGrammar {
                syntax: GrammarSyntax::Lark,
                definition: "start: SOURCE\nSOURCE: /[\\s\\S]+/".into(),
            }),
        );
        freeform.validate().unwrap();
        assert!(freeform.input_schema().is_none());
        let decoded: ToolSpec =
            serde_json::from_value(serde_json::to_value(&freeform).unwrap()).unwrap();
        assert_eq!(decoded, freeform);

        let member = ToolSpec::function_with_options(
            "lookup",
            "looks up a record",
            serde_json::json!({
                "type": "object",
                "properties": {"id": {"type": "string"}}
            }),
            false,
            true,
            Some(serde_json::json!({
                "type": "object",
                "properties": {"found": {"type": "boolean"}}
            })),
        );
        let namespace = ToolSpec::namespace("records", "record tools", vec![member]);
        namespace.validate().unwrap();
        assert!(namespace.is_deferred());
    }

    #[test]
    fn duplicate_namespace_members_are_rejected_atomically() {
        let namespace = ToolSpec::namespace(
            "records",
            "record tools",
            vec![
                ToolSpec::function("read", "one", strict_schema()),
                ToolSpec::function("read", "two", strict_schema()),
            ],
        );
        assert!(matches!(
            namespace.validate(),
            Err(ToolSpecValidationError::DuplicateNamespaceMember { namespace, tool })
                if namespace == "records" && tool == "read"
        ));
    }
}
