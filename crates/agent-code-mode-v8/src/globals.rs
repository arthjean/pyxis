//! Host globals bound into a cell's context.
//!
//! The set is an ALLOWLIST: a cell sees the standard JavaScript library minus
//! the four globals below, plus exactly the helpers listed here. There is no
//! `console`, no file system, no network and no module loader, so the only way
//! out of the isolate is a helper that Rust installed on purpose.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_code_mode::{
    CellId, CellSink, ImageDetail, NestedTool, NestedToolCall, NestedToolDispatcher,
    NestedToolInput, NestedToolOutcome, OutputItem,
};

/// Globals removed from every cell context. `Atomics` and `SharedArrayBuffer`
/// exist to share memory between agents that this design does not have;
/// `WebAssembly` would reintroduce code generation under a JITless policy; and
/// `console` writes to a stream a cell must never reach (its output goes
/// through `text()`, which the session bounds and persists).
const REMOVED_GLOBALS: [&str; 4] = ["console", "Atomics", "SharedArrayBuffer", "WebAssembly"];

/// State one cell keeps in its isolate slot for the whole of its life.
pub(crate) struct CellRuntimeState {
    pub cell: CellId,
    pub sink: CellSink,
    /// Session store as it was when the cell started.
    pub stored: HashMap<String, serde_json::Value>,
    /// Writes this cell made, merged back into the session store at the end.
    pub writes: HashMap<String, serde_json::Value>,
    pub exit_requested: bool,
    /// Nested tools the cell may call, and the pipeline that runs them.
    pub catalog: Vec<NestedTool>,
    pub dispatcher: Option<Arc<dyn NestedToolDispatcher>>,
    pub next_call: u64,
    /// Non-zero while the cell is parked inside a nested tool. The CPU
    /// watchdog stops accumulating then: a tool that takes ten seconds is not
    /// the cell burning ten seconds of CPU.
    pub in_tool: Arc<AtomicU64>,
}

impl CellRuntimeState {
    fn load(&self, key: &str) -> Option<&serde_json::Value> {
        self.writes.get(key).or_else(|| self.stored.get(key))
    }
}

pub(crate) fn install(scope: &mut v8::PinScope<'_, '_>) -> Result<(), String> {
    let global = scope.get_current_context().global(scope);
    for name in REMOVED_GLOBALS {
        let key = v8::String::new(scope, name).ok_or("cannot allocate a global name")?;
        global.delete(scope, key.into());
    }

    bind(scope, global, "text", text_callback)?;
    bind(scope, global, "image", image_callback)?;
    bind(scope, global, "audio", audio_callback)?;
    bind(scope, global, "store", store_callback)?;
    bind(scope, global, "load", load_callback)?;
    bind(scope, global, "notify", notify_callback)?;
    bind(scope, global, "yield_control", yield_control_callback)?;
    bind(scope, global, "exit", exit_callback)?;
    install_tools(scope, global)?;
    Ok(())
}

/// Builds `tools` and `ALL_TOOLS` from the catalog the cell was started with.
fn install_tools(
    scope: &mut v8::PinScope<'_, '_>,
    global: v8::Local<'_, v8::Object>,
) -> Result<(), String> {
    let catalog = scope
        .get_slot::<CellRuntimeState>()
        .map(|state| state.catalog.clone())
        .unwrap_or_default();

    let tools = v8::Object::new(scope);
    let all_tools = v8::Array::new(scope, catalog.len() as i32);
    for (index, tool) in catalog.iter().enumerate() {
        let binding = v8::String::new(scope, &tool.binding).ok_or("cannot allocate a tool name")?;
        let data = v8::Number::new(scope, index as f64);
        let function = v8::Function::builder(nested_tool_callback)
            .data(data.into())
            .build(scope)
            .ok_or_else(|| format!("cannot build the `{}` binding", tool.binding))?;
        tools.set(scope, binding.into(), function.into());

        let entry = v8::Object::new(scope);
        set_string(scope, entry, "name", &tool.name)?;
        set_string(scope, entry, "binding", &tool.binding)?;
        set_string(scope, entry, "description", &tool.description)?;
        all_tools.set_index(scope, index as u32, entry.into());
    }

    let tools_key = v8::String::new(scope, "tools").ok_or("cannot allocate `tools`")?;
    global.set(scope, tools_key.into(), tools.into());
    let catalog_key = v8::String::new(scope, "ALL_TOOLS").ok_or("cannot allocate `ALL_TOOLS`")?;
    global.set(scope, catalog_key.into(), all_tools.into());
    Ok(())
}

fn set_string(
    scope: &mut v8::PinScope<'_, '_>,
    object: v8::Local<'_, v8::Object>,
    key: &str,
    value: &str,
) -> Result<(), String> {
    let key = v8::String::new(scope, key).ok_or("cannot allocate a key")?;
    let value = v8::String::new(scope, value).ok_or("cannot allocate a value")?;
    object.set(scope, key.into(), value.into());
    Ok(())
}

fn bind(
    scope: &mut v8::PinScope<'_, '_>,
    global: v8::Local<'_, v8::Object>,
    name: &str,
    callback: impl v8::MapFnTo<v8::FunctionCallback>,
) -> Result<(), String> {
    let key = v8::String::new(scope, name).ok_or("cannot allocate a helper name")?;
    let function = v8::Function::builder(callback)
        .build(scope)
        .ok_or_else(|| format!("cannot build the `{name}` helper"))?;
    global.set(scope, key.into(), function.into());
    Ok(())
}

fn throw(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    let Some(text) = v8::String::new(scope, message) else {
        return;
    };
    let error = v8::Exception::type_error(scope, text);
    scope.throw_exception(error);
}

/// Text form of a value: a string as-is, anything else through
/// `JSON.stringify`, and the lossy string form when JSON refuses it.
fn to_text(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<'_, v8::Value>) -> String {
    if value.is_string() {
        return value.to_rust_string_lossy(scope);
    }
    match v8::json::stringify(scope, value) {
        Some(json) => json.to_rust_string_lossy(scope),
        None => value.to_rust_string_lossy(scope),
    }
}

fn to_json(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Option<serde_json::Value> {
    let json = v8::json::stringify(scope, value)?;
    serde_json::from_str(&json.to_rust_string_lossy(scope)).ok()
}

fn push(scope: &mut v8::PinScope<'_, '_>, item: OutputItem) {
    if let Some(state) = scope.get_slot::<CellRuntimeState>() {
        state.sink.push(item);
    }
}

/// Reads a string property of an object argument, if present.
fn property(
    scope: &mut v8::PinScope<'_, '_>,
    object: v8::Local<'_, v8::Object>,
    name: &str,
) -> Option<String> {
    let key = v8::String::new(scope, name)?;
    let value = object.get(scope, key.into())?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    Some(value.to_rust_string_lossy(scope))
}

fn parse_detail(value: &str) -> Option<ImageDetail> {
    match value {
        "auto" => Some(ImageDetail::Auto),
        "low" => Some(ImageDetail::Low),
        "high" => Some(ImageDetail::High),
        "original" => Some(ImageDetail::Original),
        _ => None,
    }
}

fn text_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _return_value: v8::ReturnValue<'_, v8::Value>,
) {
    let text = to_text(scope, args.get(0));
    push(scope, OutputItem::Text { text });
}

fn image_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _return_value: v8::ReturnValue<'_, v8::Value>,
) {
    let first = args.get(0);
    let (image_url, embedded_detail) = if first.is_string() {
        (first.to_rust_string_lossy(scope), None)
    } else if let Ok(object) = v8::Local::<v8::Object>::try_from(first) {
        let url = property(scope, object, "image_url");
        let detail = property(scope, object, "detail");
        match url {
            Some(url) => (url, detail),
            None => {
                throw(
                    scope,
                    "image() needs a data URL or an object with image_url",
                );
                return;
            }
        }
    } else {
        throw(
            scope,
            "image() needs a data URL or an object with image_url",
        );
        return;
    };

    let explicit = args.get(1);
    let detail = if explicit.is_undefined() || explicit.is_null() {
        embedded_detail
    } else {
        Some(explicit.to_rust_string_lossy(scope))
    };
    push(
        scope,
        OutputItem::Image {
            image_url,
            detail: detail.as_deref().and_then(parse_detail),
        },
    );
}

fn audio_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _return_value: v8::ReturnValue<'_, v8::Value>,
) {
    let first = args.get(0);
    let audio_url = if first.is_string() {
        first.to_rust_string_lossy(scope)
    } else if let Ok(object) = v8::Local::<v8::Object>::try_from(first) {
        match property(scope, object, "audio_url") {
            Some(url) => url,
            None => {
                throw(
                    scope,
                    "audio() needs a data URL or an object with audio_url",
                );
                return;
            }
        }
    } else {
        throw(
            scope,
            "audio() needs a data URL or an object with audio_url",
        );
        return;
    };
    push(scope, OutputItem::Audio { audio_url });
}

fn store_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _return_value: v8::ReturnValue<'_, v8::Value>,
) {
    let key = args.get(0);
    if !key.is_string() {
        throw(scope, "store() needs a string key");
        return;
    }
    let key = key.to_rust_string_lossy(scope);
    let Some(value) = to_json(scope, args.get(1)) else {
        throw(scope, "store() needs a JSON-serializable value");
        return;
    };
    if let Some(state) = scope.get_slot_mut::<CellRuntimeState>() {
        state.writes.insert(key, value);
    }
}

fn load_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut return_value: v8::ReturnValue<'_, v8::Value>,
) {
    let key = args.get(0);
    if !key.is_string() {
        throw(scope, "load() needs a string key");
        return;
    }
    let key = key.to_rust_string_lossy(scope);
    let Some(found) = scope
        .get_slot::<CellRuntimeState>()
        .and_then(|state| state.load(&key))
        .map(ToString::to_string)
    else {
        return_value.set_undefined();
        return;
    };
    let Some(json) = v8::String::new(scope, &found) else {
        return_value.set_undefined();
        return;
    };
    match v8::json::parse(scope, json) {
        Some(value) => return_value.set(value),
        None => return_value.set_undefined(),
    }
}

fn yield_control_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    _return_value: v8::ReturnValue<'_, v8::Value>,
) {
    if let Some(state) = scope.get_slot::<CellRuntimeState>() {
        state.sink.request_yield();
    }
}

/// Hands one value to the model right away without ending the cell.
///
/// Pyxis has one channel out of a cell, the session buffer, so `notify` is an
/// append followed by a yield. The baseline injects a second tool output
/// instead; the observable effect for the model, getting the value before the
/// cell ends, is the same.
fn notify_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _return_value: v8::ReturnValue<'_, v8::Value>,
) {
    let text = to_text(scope, args.get(0));
    if let Some(state) = scope.get_slot::<CellRuntimeState>() {
        state.sink.push(OutputItem::Text { text });
        state.sink.request_yield();
    }
}

/// `tools.<name>(input)`.
///
/// The call is dispatched SYNCHRONOUSLY on the cell thread and the answer comes
/// back as an already-settled promise. That is what makes the terminal order of
/// a cell's nested calls the order the cell made them, and it costs nothing the
/// runtime needs: the cell thread is a plain OS thread, so parking it parks
/// neither Tokio nor another cell.
fn nested_tool_callback(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut return_value: v8::ReturnValue<'_, v8::Value>,
) {
    let index = args.data().number_value(scope).unwrap_or(-1.0);
    let Some(index) = (index >= 0.0).then_some(index as usize) else {
        throw(scope, "this nested tool binding is not usable");
        return;
    };

    let Some((tool, freeform, cell, dispatcher, in_tool)) =
        scope.get_slot::<CellRuntimeState>().and_then(|state| {
            let entry = state.catalog.get(index)?;
            Some((
                entry.name.clone(),
                entry.freeform,
                state.cell.clone(),
                state.dispatcher.clone(),
                Arc::clone(&state.in_tool),
            ))
        })
    else {
        throw(scope, "this nested tool binding is not usable");
        return;
    };

    let call_id = match scope.get_slot_mut::<CellRuntimeState>() {
        Some(state) => {
            state.next_call += 1;
            format!("nested-{}", state.next_call)
        }
        None => "nested-0".to_string(),
    };

    let raw = args.get(0);
    let input = if freeform {
        // A freeform tool takes TEXT. Handing it a JSON object would be the
        // invented schema the tool algebra forbids.
        NestedToolInput::Text(if raw.is_undefined() {
            String::new()
        } else {
            to_text(scope, raw)
        })
    } else if raw.is_undefined() || raw.is_null() {
        NestedToolInput::Json(serde_json::json!({}))
    } else if raw.is_string() {
        NestedToolInput::Json(serde_json::Value::String(raw.to_rust_string_lossy(scope)))
    } else {
        match to_json(scope, raw) {
            Some(value) => NestedToolInput::Json(value),
            None => {
                throw(scope, "a nested tool input must be JSON-serializable");
                return;
            }
        }
    };

    let call = NestedToolCall {
        cell_id: cell,
        call_id,
        tool,
        input,
    };
    let outcome = match dispatcher {
        Some(dispatcher) => {
            in_tool.fetch_add(1, Ordering::AcqRel);
            let outcome = dispatcher.dispatch(call);
            in_tool.fetch_sub(1, Ordering::AcqRel);
            outcome
        }
        None => NestedToolOutcome::error(
            &call,
            "unknown_tool",
            "no nested tool is available in this cell",
        ),
    };

    let Some(resolver) = v8::PromiseResolver::new(scope) else {
        throw(scope, "cannot allocate the nested tool promise");
        return;
    };
    let promise = resolver.get_promise(scope);
    if outcome.is_error {
        let error = build_tool_error(scope, &outcome);
        resolver.reject(scope, error);
    } else {
        let value = match &outcome.structured {
            Some(structured) => json_to_value(scope, structured),
            None => v8::String::new(scope, &outcome.content).map(Into::into),
        };
        match value {
            Some(value) => resolver.resolve(scope, value),
            None => resolver.resolve(scope, v8::undefined(scope).into()),
        };
    }
    return_value.set(promise.into());
}

/// Structured rejection: the cell can branch on `kind` instead of parsing a
/// message, which is what US-008 means by a structured error coming back.
fn build_tool_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    outcome: &NestedToolOutcome,
) -> v8::Local<'s, v8::Value> {
    let message =
        v8::String::new(scope, &outcome.content).unwrap_or_else(|| v8::String::empty(scope));
    let error = v8::Exception::error(scope, message);
    if let Ok(object) = v8::Local::<v8::Object>::try_from(error) {
        let _ = set_string(scope, object, "name", "ToolError");
        let _ = set_string(scope, object, "tool", &outcome.tool);
        // Every failed outcome carries its category, so nothing is invented
        // here: a missing one stays missing rather than being papered over
        // with a second default the dispatcher already applied.
        if let Some(kind) = outcome.error_kind {
            let _ = set_string(scope, object, "kind", kind);
        }
    }
    error
}

fn json_to_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &serde_json::Value,
) -> Option<v8::Local<'s, v8::Value>> {
    let text = v8::String::new(scope, &value.to_string())?;
    v8::json::parse(scope, text)
}

/// Marker carried by the exception `exit()` raises.
///
/// The exit has to be an exception rather than an isolate termination: V8 only
/// observes a termination request at a stack-guard check (a loop back-edge, a
/// call into a new frame), so straight-line code after `exit()` would keep
/// running. A throw stops at the statement itself. The marker is what lets the
/// classifier tell an exit apart from a real error the model raised after
/// catching one.
pub(crate) const EXIT_MARKER: &str = "__pyxis_code_mode_exit__";

/// Ends the cell successfully from the inside, like an early return.
fn exit_callback(
    scope: &mut v8::PinScope<'_, '_>,
    _args: v8::FunctionCallbackArguments<'_>,
    _return_value: v8::ReturnValue<'_, v8::Value>,
) {
    if let Some(state) = scope.get_slot_mut::<CellRuntimeState>() {
        state.exit_requested = true;
    }
    // Also requested, so a `catch` that swallows the throw still stops at the
    // next loop back-edge instead of running unbounded.
    scope.terminate_execution();
    let Some(message) = v8::String::new(scope, EXIT_MARKER) else {
        return;
    };
    let error = v8::Exception::error(scope, message);
    scope.throw_exception(error);
}
