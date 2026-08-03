//! Model-visible surface of Code Mode: the freeform `exec` tool and the
//! function `wait` tool, plus the pragma `exec` accepts on its first line.
//!
//! The grammar and the shape of both tools are ADOPTED from the Codex baseline
//! (`core/src/tools/code_mode/{execute_spec,wait_spec}.rs` at
//! `fa1d4c40d0e63eef2e0ba8a9e004ccd0a80b77f5`); the descriptions are rewritten
//! against Pyxis semantics. Provenance is recorded in `NOTICE-CODEX.md` and
//! `docs/codex-port-inventory.md`.

use agent_core::provider::{GrammarSyntax, ToolGrammar, ToolKind, ToolSpec};

use crate::protocol::{DEFAULT_YIELD_TIME, NestedTool};

pub const EXEC_TOOL_NAME: &str = "exec";
pub const WAIT_TOOL_NAME: &str = "wait";
/// First-line pragma `exec` accepts, verbatim from the baseline.
pub const EXEC_PRAGMA_PREFIX: &str = "// @exec:";
/// Default token budget of one direct `exec` result, from the baseline.
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
/// Description lines of a nested tool kept in the rendered catalog.
const CATALOG_DESCRIPTION_LINES: usize = 4;

/// Lark grammar of the `exec` input, adopted verbatim from the baseline so a
/// model trained on Codex sends bytes Pyxis accepts unchanged.
pub const EXEC_GRAMMAR: &str = r#"
start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

/// What the optional first-line pragma may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExecPragma {
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<usize>,
}

/// Why an `exec` input was refused before a cell was created.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecInputError {
    #[error("exec needs JavaScript source, not an empty input")]
    EmptySource,
    #[error("the `{EXEC_PRAGMA_PREFIX}` pragma is not valid JSON: {detail}")]
    MalformedPragma { detail: String },
    #[error("the `{EXEC_PRAGMA_PREFIX}` pragma field `{field}` is not a positive integer")]
    InvalidPragmaField { field: &'static str },
}

/// Splits an `exec` input into its optional pragma and its JavaScript source.
///
/// The pragma is refused rather than ignored when it is malformed: silently
/// dropping it would run the cell under budgets the model did not ask for.
pub fn parse_exec_source(input: &str) -> Result<(ExecPragma, &str), ExecInputError> {
    let trimmed = input.trim_start_matches(['\u{feff}']);
    // A pragma is a FIRST line and nothing else, so the only question is
    // whether the first line carries it; a source without a newline is its own
    // first line.
    let (first, rest) = trimmed.split_once('\n').unwrap_or((trimmed, ""));
    let pragma_line = first.trim_start();
    let (pragma, source) = if pragma_line.starts_with(EXEC_PRAGMA_PREFIX) {
        (read_pragma(pragma_line)?, rest)
    } else {
        (ExecPragma::default(), trimmed)
    };

    if source.trim().is_empty() {
        return Err(ExecInputError::EmptySource);
    }
    Ok((pragma, source))
}

fn read_pragma(line: &str) -> Result<ExecPragma, ExecInputError> {
    let payload = line.trim_start_matches(EXEC_PRAGMA_PREFIX).trim();
    let value: serde_json::Value =
        serde_json::from_str(payload).map_err(|error| ExecInputError::MalformedPragma {
            detail: error.to_string(),
        })?;
    let Some(object) = value.as_object() else {
        return Err(ExecInputError::MalformedPragma {
            detail: "expected a JSON object".into(),
        });
    };
    Ok(ExecPragma {
        yield_time_ms: read_positive(object, "yield_time_ms")?,
        max_output_tokens: read_positive(object, "max_output_tokens")?.map(|value| value as usize),
    })
}

fn read_positive(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<Option<u64>, ExecInputError> {
    match object.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|number| *number > 0)
            .map(Some)
            .ok_or(ExecInputError::InvalidPragmaField { field }),
    }
}

/// Builds the freeform `exec` tool, catalog included.
pub fn exec_tool_spec(catalog: &[NestedTool], code_mode_only: bool) -> ToolSpec {
    ToolSpec {
        name: EXEC_TOOL_NAME.to_string(),
        description: exec_description(catalog, code_mode_only),
        kind: ToolKind::Freeform {
            grammar: Some(ToolGrammar {
                syntax: GrammarSyntax::Lark,
                definition: EXEC_GRAMMAR.to_string(),
            }),
        },
    }
}

/// Builds the function `wait` tool.
///
/// Every property is `required` with a nullable type rather than optional:
/// `ToolSpec::validate` demands a strict schema, and a strict schema says
/// "absent" with `null`, never by leaving a key out.
///
/// The output budget is deliberately NOT a parameter here. It is set once, by
/// the `exec` that opened the cell, and the session restores it at every yield;
/// a second knob on `wait` would be a promise this crate does not keep.
pub fn wait_tool_spec() -> ToolSpec {
    ToolSpec::function(
        WAIT_TOOL_NAME,
        wait_description(),
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["cell_id", "yield_time_ms", "terminate"],
            "properties": {
                "cell_id": {
                    "type": "string",
                    "description": "Identifier of the running exec cell."
                },
                "yield_time_ms": {
                    "type": ["integer", "null"],
                    "description": "Wait this long for more output before yielding again. Defaults to 10000 ms."
                },
                "terminate": {
                    "type": ["boolean", "null"],
                    "description": "True stops the running exec cell; false or null waits for output."
                }
            }
        }),
    )
}

fn exec_description(catalog: &[NestedTool], code_mode_only: bool) -> String {
    let yield_time = DEFAULT_YIELD_TIME.as_millis();
    let mut description = format!(
        "Run JavaScript to orchestrate and compose tool calls.\n\
- Evaluates the input in a fresh V8 isolate, as the body of an async function, so `await` works at top level.\n\
- Raw JavaScript source only: not JSON, not a quoted string, not a markdown code fence.\n\
- No Node, no module loader, no file system, no network, no console. The only way out of the isolate is a helper below.\n\
- Values do NOT survive a cell through the global object: use `store` and `load`, which are scoped to this thread's session.\n\
- The first line may carry a pragma, for example `// @exec: {{\"yield_time_ms\": 10000, \"max_output_tokens\": 1000}}`. A malformed pragma is refused, never ignored.\n\
- `yield_time_ms` asks `exec` to hand back what the cell produced if it is still running. Defaults to {yield_time} ms.\n\
- `max_output_tokens` bounds the direct result of this call. Defaults to {DEFAULT_MAX_OUTPUT_TOKENS} tokens.\n\
\n\
Helpers:\n\
- `text(value)`: appends a text item. A non-string is stringified with `JSON.stringify`.\n\
- `image(urlOrItem, detail?)`: appends an image item. `image_url` must be a base64 `data:` URL.\n\
- `audio(urlOrItem)`: appends an audio item, same rule for its URL.\n\
- `store(key, value)` / `load(key)`: session-scoped values, shared by the cells of this thread only.\n\
- `notify(value)`: hands the accumulated output to the model right away without ending the cell.\n\
- `yield_control()`: same, for output already produced.\n\
- `exit()`: ends the cell successfully, like an early return.\n\
- `ALL_TOOLS`: `{{ name, description }}` for every nested tool.\n"
    );

    if code_mode_only {
        description.push_str(
            "\nThis model orchestrates through `exec` only: the tools below are NOT callable \
directly, they are callable from JavaScript on the `tools` object.\n",
        );
    }
    description.push_str(&render_catalog(catalog));
    description
}

/// Renders the nested catalog as TypeScript-ish signatures on `tools`.
fn render_catalog(catalog: &[NestedTool]) -> String {
    if catalog.is_empty() {
        return "\nNo nested tool is available in this cell.\n".to_string();
    }
    let mut rendered = String::from("\nNested tools, on the global `tools` object:\n\n```ts\n");
    let mut sorted: Vec<&NestedTool> = catalog.iter().collect();
    sorted.sort_by(|left, right| left.binding.cmp(&right.binding));
    for tool in sorted {
        let mut lines = tool.description.lines();
        for line in lines.by_ref().take(CATALOG_DESCRIPTION_LINES) {
            rendered.push_str("// ");
            rendered.push_str(line);
            rendered.push('\n');
        }
        // A cut description SAYS it was cut: a model must not read the visible
        // part as the whole contract of the tool.
        if lines.next().is_some() {
            rendered.push_str("// [description truncated]\n");
        }
        if tool.freeform {
            // A freeform tool takes TEXT: giving it an object argument would be
            // the invented `input_schema` US-002 forbids.
            rendered.push_str(&format!(
                "declare function {}(input: string): Promise<string>;\n\n",
                tool.binding
            ));
        } else {
            let output = tool
                .output_schema
                .as_ref()
                .map(|schema| schema_to_typescript(Some(schema)))
                .unwrap_or_else(|| "string".to_string());
            rendered.push_str(&format!(
                "declare function {}(input: {}): Promise<{}>;\n\n",
                tool.binding,
                schema_to_typescript(tool.input_schema.as_ref()),
                output,
            ));
        }
    }
    rendered.push_str("```\n");
    rendered
}

/// Minimal JSON Schema to TypeScript projection: enough for a model to know
/// the property names and their kinds, without pretending to cover the whole
/// specification.
fn schema_to_typescript(schema: Option<&serde_json::Value>) -> String {
    let Some(object) = schema.and_then(|schema| schema.as_object()) else {
        return "Record<string, unknown>".to_string();
    };
    let Some(properties) = object.get("properties").and_then(|value| value.as_object()) else {
        return "Record<string, unknown>".to_string();
    };
    if properties.is_empty() {
        return "Record<string, never>".to_string();
    }
    let required: Vec<&str> = object
        .get("required")
        .and_then(|value| value.as_array())
        .map(|values| values.iter().filter_map(|value| value.as_str()).collect())
        .unwrap_or_default();
    let mut fields: Vec<String> = Vec::with_capacity(properties.len());
    for (name, property) in properties {
        let optional = if required.contains(&name.as_str()) {
            ""
        } else {
            "?"
        };
        fields.push(format!(
            "{name}{optional}: {}",
            json_type_to_typescript(property)
        ));
    }
    format!("{{ {} }}", fields.join("; "))
}

fn json_type_to_typescript(property: &serde_json::Value) -> String {
    let kind = property.get("type");
    let one = |name: &str| match name {
        "string" => "string",
        "integer" | "number" => "number",
        "boolean" => "boolean",
        "array" => "unknown[]",
        "object" => "Record<string, unknown>",
        "null" => "null",
        _ => "unknown",
    };
    match kind {
        Some(serde_json::Value::String(name)) => one(name).to_string(),
        Some(serde_json::Value::Array(names)) => {
            // Deduplicated in the order the schema lists them: `integer` and
            // `number` collapse to the same TypeScript type without being
            // adjacent, which a sort-free `dedup` would have missed.
            let mut parts: Vec<&str> = Vec::with_capacity(names.len());
            for part in names.iter().filter_map(|name| name.as_str()).map(one) {
                if !parts.contains(&part) {
                    parts.push(part);
                }
            }
            if parts.is_empty() {
                "unknown".to_string()
            } else {
                parts.join(" | ")
            }
        }
        _ => "unknown".to_string(),
    }
}

fn wait_description() -> String {
    format!(
        "Resumes a yielded `{EXEC_TOOL_NAME}` cell and returns its NEW output.\n\
- Use it only after `{EXEC_TOOL_NAME}` answered with a cell identifier.\n\
- `cell_id` names the cell to resume; a cell of another thread is refused.\n\
- `yield_time_ms` bounds this wait. Defaults to {} ms.\n\
- `terminate: true` stops the cell instead of waiting for it.\n\
- Only the output produced since the previous yield comes back, never a repeat.\n\
- A finished cell returns its result once and is then closed.",
        DEFAULT_YIELD_TIME.as_millis()
    )
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
