//! Canonical task names of the agent tree (US-011).
//!
//! A v2 model addresses a child by NAME, not by opaque identifier: it writes
//! `spawn_agent(task_name: "reader")` then `followup_task(target: "reader")`.
//! The name has to be stable across a restart, unique inside a parent, and
//! impossible to confuse with a path into someone else's subtree, which is what
//! this type is for.
//!
//! Shape and validation rules follow the baseline (`codex-rs/protocol/src/agent_path.rs`,
//! `inspired` in `docs/codex-port-inventory.md`): an absolute path starts at
//! `/root`, each segment is lowercase ASCII, digits and underscores, and `root`,
//! `.` and `..` are reserved. Pyxis adds its own bounds (segment and path
//! length) because a name reaches a durable log and a tool result.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Canonical path of the thread a user drives. Every child hangs under it.
pub const ROOT_PATH: &str = "/root";
const ROOT_SEGMENT: &str = "root";
/// Characters one segment may carry. A name is a handle for a model, not a
/// place to smuggle a paragraph.
pub const MAX_AGENT_NAME: usize = 64;
/// Characters a whole path may carry, bounding the depth a reference can claim
/// before anything walks it.
pub const MAX_AGENT_PATH: usize = 512;

/// Why a name or a reference was refused. Always BEFORE anything is created:
/// an invalid target must never reach the graph (US-013 AC3).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentPathError {
    #[error("agent task name must not be empty")]
    Empty,
    #[error("agent task name `{0}` is reserved")]
    Reserved(String),
    #[error(
        "agent task name `{0}` must use only lowercase letters, digits and underscores, and no `/`"
    )]
    InvalidCharacter(String),
    #[error("agent task name must not exceed {MAX_AGENT_NAME} characters")]
    NameTooLong,
    #[error("agent path must not exceed {MAX_AGENT_PATH} characters")]
    PathTooLong,
    #[error("absolute agent paths must start with `{ROOT_PATH}`")]
    NotAbsolute,
}

/// An absolute, canonical position in the agent tree: `/root`, `/root/reader`,
/// `/root/reader/notes`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentPath(String);

impl AgentPath {
    pub fn root() -> Self {
        Self(ROOT_PATH.to_string())
    }

    /// Parses an ABSOLUTE path. A relative reference is refused here on
    /// purpose: resolving it requires knowing who is asking, which is
    /// [`AgentPath::resolve`]'s job.
    pub fn parse(raw: &str) -> Result<Self, AgentPathError> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(AgentPathError::Empty);
        }
        if raw.len() > MAX_AGENT_PATH {
            return Err(AgentPathError::PathTooLong);
        }
        if raw == ROOT_PATH {
            return Ok(Self::root());
        }
        let stripped = raw.strip_prefix('/').ok_or(AgentPathError::NotAbsolute)?;
        let mut segments = stripped.split('/');
        if segments.next() != Some(ROOT_SEGMENT) {
            return Err(AgentPathError::NotAbsolute);
        }
        for segment in segments {
            validate_name(segment)?;
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0 == ROOT_PATH
    }

    /// Last segment: what a listing shows and what a sibling addresses.
    pub fn name(&self) -> &str {
        if self.is_root() {
            return ROOT_SEGMENT;
        }
        self.0.rsplit('/').next().unwrap_or(ROOT_SEGMENT)
    }

    /// Child of this path called `name`.
    pub fn join(&self, name: &str) -> Result<Self, AgentPathError> {
        validate_name(name.trim())?;
        Self::parse(&format!("{}/{}", self.0, name.trim()))
    }

    /// Resolves what a model wrote, from THIS path: an absolute path is taken
    /// as-is, anything else is read as a descendant of this one. A model that
    /// says `reader` from `/root` means `/root/reader` and can never mean a
    /// path outside its own subtree.
    pub fn resolve(&self, reference: &str) -> Result<Self, AgentPathError> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(AgentPathError::Empty);
        }
        if reference.starts_with('/') {
            return Self::parse(reference);
        }
        let mut path = self.clone();
        for segment in reference.split('/') {
            path = path.join(segment)?;
        }
        Ok(path)
    }

    /// Is `other` this path or below it? The refusal that keeps a parent from
    /// addressing another parent's subtree is written in these terms.
    pub fn contains(&self, other: &Self) -> bool {
        other.0 == self.0 || other.0.starts_with(&format!("{}/", self.0))
    }
}

fn validate_name(name: &str) -> Result<(), AgentPathError> {
    if name.is_empty() {
        return Err(AgentPathError::Empty);
    }
    if name.len() > MAX_AGENT_NAME {
        return Err(AgentPathError::NameTooLong);
    }
    if name == ROOT_SEGMENT || name == "." || name == ".." {
        return Err(AgentPathError::Reserved(name.to_string()));
    }
    if !name.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
    }) {
        return Err(AgentPathError::InvalidCharacter(bounded(name)));
    }
    Ok(())
}

/// Bounds and de-fangs what goes back into an error a model reads.
fn bounded(value: &str) -> String {
    value
        .chars()
        .take(MAX_AGENT_NAME)
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect()
}

impl fmt::Display for AgentPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for AgentPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AgentPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_canonical_name_hangs_under_the_root_and_keeps_its_last_segment() {
        let root = AgentPath::root();
        assert!(root.is_root());
        assert_eq!(root.name(), "root");

        let child = root.join("reader").unwrap();
        assert_eq!(child.as_str(), "/root/reader");
        assert_eq!(child.name(), "reader");
        assert!(root.contains(&child));
        assert!(!child.contains(&root));
    }

    /// A reference a model writes is resolved from ITS position, so a relative
    /// name can never escape the subtree of the agent that wrote it.
    #[test]
    fn a_relative_reference_stays_inside_the_subtree_of_the_caller() {
        let base = AgentPath::parse("/root/reader").unwrap();
        assert_eq!(
            base.resolve("notes").unwrap().as_str(),
            "/root/reader/notes"
        );
        assert_eq!(base.resolve("/root/other").unwrap().as_str(), "/root/other");
        assert!(matches!(
            base.resolve(".."),
            Err(AgentPathError::Reserved(_))
        ));
        assert!(matches!(base.resolve(""), Err(AgentPathError::Empty)));
    }

    /// Everything a name may not be, refused with its own cause rather than a
    /// single opaque "invalid".
    #[test]
    fn reserved_shaped_and_oversized_names_are_refused_by_cause() {
        assert!(matches!(
            AgentPath::root().join("root"),
            Err(AgentPathError::Reserved(_))
        ));
        assert!(matches!(
            AgentPath::root().join("Reader"),
            Err(AgentPathError::InvalidCharacter(_))
        ));
        assert!(matches!(
            AgentPath::root().join("a b"),
            Err(AgentPathError::InvalidCharacter(_))
        ));
        assert!(matches!(
            AgentPath::root().join("a/b"),
            Err(AgentPathError::InvalidCharacter(_))
        ));
        assert!(matches!(
            AgentPath::root().join(&"x".repeat(MAX_AGENT_NAME + 1)),
            Err(AgentPathError::NameTooLong)
        ));
        assert!(matches!(
            AgentPath::parse("reader"),
            Err(AgentPathError::NotAbsolute)
        ));
        assert!(matches!(
            AgentPath::parse("/other/reader"),
            Err(AgentPathError::NotAbsolute)
        ));
        assert!(matches!(
            AgentPath::parse(&format!("/root/{}", "a".repeat(MAX_AGENT_PATH))),
            Err(AgentPathError::PathTooLong)
        ));
    }

    /// A name reaches a durable log: it must survive the file, and a corrupt
    /// line must fail to load rather than resurrect an unvalidated path.
    #[test]
    fn a_path_round_trips_through_json_and_rejects_a_corrupt_line() {
        let path = AgentPath::parse("/root/reader_2").unwrap();
        let line = serde_json::to_string(&path).unwrap();
        assert_eq!(line, "\"/root/reader_2\"");
        assert_eq!(serde_json::from_str::<AgentPath>(&line).unwrap(), path);
        assert!(serde_json::from_str::<AgentPath>("\"/evil\"").is_err());
    }
}
