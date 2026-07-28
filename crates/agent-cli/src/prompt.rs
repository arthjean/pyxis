//! The system prompt selected by the effective model runtime.

use agent_core::model::ResolvedModelRuntime;

pub fn select_system_prompt(runtime: &ResolvedModelRuntime) -> &str {
    &runtime.instructions
}
