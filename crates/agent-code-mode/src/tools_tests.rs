//! US-008 proofs that need no JavaScript engine: the input grammar, the pragma
//! and the two model-visible specs.

use agent_core::provider::{GrammarSyntax, ToolKind};

use super::*;

#[test]
fn plain_source_carries_no_pragma() {
    let (pragma, source) = parse_exec_source("text('hi');").unwrap();
    assert_eq!(pragma, ExecPragma::default());
    assert_eq!(source, "text('hi');");
}

#[test]
fn a_first_line_pragma_is_read_and_removed_from_the_source() {
    let (pragma, source) = parse_exec_source(
        "// @exec: {\"yield_time_ms\": 500, \"max_output_tokens\": 32}\ntext(1);",
    )
    .unwrap();
    assert_eq!(pragma.yield_time_ms, Some(500));
    assert_eq!(pragma.max_output_tokens, Some(32));
    assert_eq!(source, "text(1);");
}

#[test]
fn a_comment_that_is_not_the_pragma_stays_in_the_source() {
    let (pragma, source) = parse_exec_source("// ordinary comment\ntext(1);").unwrap();
    assert_eq!(pragma, ExecPragma::default());
    assert_eq!(source, "// ordinary comment\ntext(1);");
}

/// A budget the model asked for and did not get would be a silent downgrade.
#[test]
fn a_malformed_pragma_is_refused_rather_than_ignored() {
    assert!(matches!(
        parse_exec_source("// @exec: {not json}\ntext(1);"),
        Err(ExecInputError::MalformedPragma { .. })
    ));
    assert!(matches!(
        parse_exec_source("// @exec: {\"yield_time_ms\": 0}\ntext(1);"),
        Err(ExecInputError::InvalidPragmaField {
            field: "yield_time_ms"
        })
    ));
    assert!(matches!(
        parse_exec_source("// @exec: {\"max_output_tokens\": -3}\ntext(1);"),
        Err(ExecInputError::InvalidPragmaField {
            field: "max_output_tokens"
        })
    ));
}

#[test]
fn an_empty_or_pragma_only_input_is_refused() {
    assert_eq!(
        parse_exec_source("   \n  "),
        Err(ExecInputError::EmptySource)
    );
    assert_eq!(
        parse_exec_source("// @exec: {\"yield_time_ms\": 10}"),
        Err(ExecInputError::EmptySource)
    );
}

#[test]
fn exec_is_a_freeform_tool_carrying_the_baseline_grammar() {
    let spec = exec_tool_spec(&[], true);
    spec.validate().expect("the exec spec must be valid");
    assert!(spec.is_freeform());
    assert_eq!(spec.input_schema(), None, "a freeform tool has no schema");
    let ToolKind::Freeform { grammar } = &spec.kind else {
        unreachable!("exec must be freeform");
    };
    let grammar = grammar.as_ref().expect("exec carries a grammar");
    assert_eq!(grammar.syntax, GrammarSyntax::Lark);
    assert!(grammar.definition.contains("PRAGMA_LINE"));
}

#[test]
fn wait_is_a_strict_function_tool() {
    let spec = wait_tool_spec();
    spec.validate().expect("the wait spec must be valid");
    assert!(!spec.is_freeform());
    let schema = spec.input_schema().expect("wait takes JSON");
    assert_eq!(schema["additionalProperties"], serde_json::json!(false));
}

#[test]
fn the_catalog_projects_function_and_freeform_tools_differently() {
    let function = ToolSpec::function_with_options(
        "read-file",
        "Reads a file.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path", "limit"],
            "properties": {
                "path": { "type": "string" },
                "limit": { "type": ["integer", "null"] }
            }
        }),
        true,
        false,
        Some(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["output", "exit_code"],
            "properties": {
                "output": { "type": "string" },
                "exit_code": { "type": ["integer", "null"] }
            }
        })),
    );
    let freeform = ToolSpec::freeform("apply_patch", "Applies a patch.", None);

    let catalog = NestedTool::catalog(&[function, freeform]);
    let description = exec_tool_spec(&catalog, true).description;
    assert!(
        description.contains(
            "declare function read_file(input: { limit: number | null; path: string }): Promise<{ exit_code: number | null; output: string }>;"
        ),
        "{description}"
    );
    assert!(
        description.contains("declare function apply_patch(input: string): Promise<string>;"),
        "a freeform tool must keep a text input: {description}"
    );
}

#[test]
fn an_empty_catalog_says_so_instead_of_pretending() {
    let description = exec_tool_spec(&[], false).description;
    assert!(description.contains("No nested tool is available in this cell."));
}

/// Two names that normalize to the same JavaScript identifier are BOTH
/// callable: without this, the second binding overwrites the first on the
/// `tools` object and the model calls one tool believing it called the other.
/// `mcp__server__list-files` next to `mcp__server__list_files` is the real
/// shape of it, since `agent-mcp` keeps the dash it was given.
#[test]
fn colliding_tool_names_get_distinct_bindings() {
    let catalog = NestedTool::catalog(&[
        ToolSpec::freeform("mcp__srv__list-files", "Dashed.", None),
        ToolSpec::freeform("mcp__srv__list_files", "Underscored.", None),
        ToolSpec::freeform("mcp__srv__list_files_1", "Already taken.", None),
    ]);
    let bindings: Vec<&str> = catalog.iter().map(|tool| tool.binding.as_str()).collect();
    assert_eq!(
        bindings,
        vec![
            "mcp__srv__list_files",
            "mcp__srv__list_files_1",
            "mcp__srv__list_files_1_1"
        ],
        "a taken fallback is skipped rather than colliding in turn"
    );
    // The names themselves are untouched: only the JavaScript identifier moves.
    let names: Vec<&str> = catalog.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "mcp__srv__list-files",
            "mcp__srv__list_files",
            "mcp__srv__list_files_1"
        ]
    );

    let description = exec_tool_spec(&catalog, false).description;
    for binding in bindings {
        assert!(
            description.contains(&format!("declare function {binding}(")),
            "each tool must be reachable under its own binding: {description}"
        );
    }
}

/// The budget belongs to the `exec` that opened the cell. Advertising a second
/// one on `wait` would be a parameter nothing reads.
#[test]
fn wait_advertises_no_output_budget_it_does_not_apply() {
    let schema = wait_tool_spec()
        .input_schema()
        .expect("wait takes JSON")
        .clone();
    assert_eq!(
        schema["required"],
        serde_json::json!(["cell_id", "yield_time_ms", "terminate"])
    );
    assert!(
        schema["properties"].get("max_output_bytes").is_none(),
        "{schema}"
    );
}
