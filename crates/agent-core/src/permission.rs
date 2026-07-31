//! Permission mode: the one axis of the permission model that belongs to the
//! CONTRACT rather than to the tool pipeline (ARCHITECTURE 4.4).
//!
//! It lives here, and not in `agent-tools`, because a permission request is an
//! event the core hands to its clients, and the mode a request was raised under
//! is part of what the client must be able to read. Passing it as a debug-
//! formatted string, which is what this module replaces, made every consumer
//! parse prose to recover a closed set of five values.
//!
//! Codex expresses the same axis as `AskForApproval`
//! (`codex-rs/protocol/src/protocol.rs:910`). The VALUES differ on purpose:
//! Pyxis owns its five modes and does not port Codex's approval policies.

use serde::{Deserialize, Serialize};

/// The 5 permission modes (ARCHITECTURE 4.4).
///
/// The serialized spelling is the one users already type on the command line
/// and read in `/permissions`. Naming the same mode two ways, one for humans and
/// one for the wire, is what a single closed type is here to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PermissionMode {
    /// Asks on a sensitive action.
    #[default]
    #[serde(rename = "ask")]
    Default,
    /// Auto-accepts file edits, asks for the rest.
    #[serde(rename = "accept-edits")]
    AcceptEdits,
    /// Never interrupts (controlled automations).
    #[serde(rename = "auto")]
    DontAsk,
    /// Short-circuits every check (advanced use / under a sandbox).
    #[serde(rename = "full-access")]
    BypassPermissions,
    /// Read-only: no mutation allowed (planning phase).
    #[serde(rename = "read-only")]
    Plan,
}

impl PermissionMode {
    /// Stable identifier: the spelling configuration, the CLI flag, the
    /// `/permissions` picker and the wire all share.
    pub fn id(self) -> &'static str {
        match self {
            Self::Default => "ask",
            Self::AcceptEdits => "accept-edits",
            Self::DontAsk => "auto",
            Self::BypassPermissions => "full-access",
            Self::Plan => "read-only",
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One spelling per mode, shared by the CLI, the picker and the wire.
    #[test]
    fn ids_are_the_user_facing_spelling() {
        assert_eq!(PermissionMode::default(), PermissionMode::Default);
        assert_eq!(PermissionMode::BypassPermissions.id(), "full-access");
        assert_eq!(
            serde_json::to_string(&PermissionMode::AcceptEdits).unwrap(),
            "\"accept-edits\""
        );
        assert_eq!(PermissionMode::Plan.to_string(), "read-only");
    }

    #[test]
    fn every_mode_round_trips_through_its_id() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::DontAsk,
            PermissionMode::BypassPermissions,
            PermissionMode::Plan,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{}\"", mode.id()));
            assert_eq!(
                serde_json::from_str::<PermissionMode>(&json).unwrap(),
                mode,
                "{mode} must survive the wire"
            );
        }
    }
}
