//! Durable identifiers of the runtime (US-002).
//!
//! One shape for all of them: a 128-bit opaque payload rendered as
//! `<prefix>_<32 lowercase hex>`. The prefix is what makes a `TurnId` passed
//! where a `ThreadId` is expected fail at DESERIALIZATION and not only at
//! compile time (an identifier crossing a JSONL file loses its Rust type).
//!
//! No `uuid` dependency (PRD open question): the value has to be opaque,
//! serializable, comparable and unique, none of which requires a version
//! layout. `rand` was already in the workspace, and generation goes through an
//! injected `IdGenerator` so deterministic tests do not depend on a global
//! clock or on entropy.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Raw payload of every identifier.
pub const ID_BYTES: usize = 16;
const ID_HEX_LEN: usize = ID_BYTES * 2;

/// Source of identifier payloads. Injected so a test can replace entropy with a
/// counter and replay the exact same identifiers.
pub trait IdGenerator: Send + Sync {
    fn next_bytes(&self) -> [u8; ID_BYTES];
}

/// Production generator: 128 random bits per identifier.
#[derive(Debug, Clone, Copy, Default)]
pub struct RandomIds;

impl IdGenerator for RandomIds {
    fn next_bytes(&self) -> [u8; ID_BYTES] {
        use rand::RngCore;
        let mut bytes = [0u8; ID_BYTES];
        rand::rng().fill_bytes(&mut bytes);
        bytes
    }
}

/// Deterministic generator: a big-endian counter in the low 64 bits. Two runs
/// seeded the same way produce byte-identical identifiers.
#[derive(Debug)]
pub struct SequentialIds {
    next: AtomicU64,
}

impl SequentialIds {
    pub fn new() -> Self {
        Self::starting_at(1)
    }

    pub fn starting_at(first: u64) -> Self {
        Self {
            next: AtomicU64::new(first),
        }
    }
}

impl Default for SequentialIds {
    fn default() -> Self {
        Self::new()
    }
}

impl IdGenerator for SequentialIds {
    fn next_bytes(&self) -> [u8; ID_BYTES] {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let mut bytes = [0u8; ID_BYTES];
        bytes[8..].copy_from_slice(&n.to_be_bytes());
        bytes
    }
}

/// Why a textual identifier was rejected. Validation NEVER panics: a corrupt
/// durable file must surface a named error, not abort the process.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("empty identifier")]
    Empty,
    #[error("expected an identifier prefixed `{expected}`, got `{got}`")]
    Prefix { expected: &'static str, got: String },
    #[error("expected {ID_HEX_LEN} hex characters after `{prefix}`, got {got}")]
    Length { prefix: &'static str, got: usize },
    #[error("invalid hex payload after `{prefix}`")]
    Hex { prefix: &'static str },
}

/// Bounds what an error message echoes back: the input can come from a
/// corrupt file, and an error is not a place to reproduce arbitrary content.
fn echo(raw: &str) -> String {
    raw.chars().take(16).collect()
}

fn parse_payload(raw: &str, prefix: &'static str) -> Result<[u8; ID_BYTES], IdError> {
    if raw.is_empty() {
        return Err(IdError::Empty);
    }
    let Some(hex_part) = raw.strip_prefix(prefix) else {
        return Err(IdError::Prefix {
            expected: prefix,
            got: echo(raw),
        });
    };
    if hex_part.len() != ID_HEX_LEN {
        return Err(IdError::Length {
            prefix,
            got: hex_part.len(),
        });
    }
    let mut bytes = [0u8; ID_BYTES];
    hex::decode_to_slice(hex_part, &mut bytes).map_err(|_| IdError::Hex { prefix })?;
    // `hex::decode_to_slice` accepts uppercase; the canonical form is lowercase,
    // so that equality on the textual form matches equality on the payload.
    if hex_part.chars().any(|c| c.is_ascii_uppercase()) {
        return Err(IdError::Hex { prefix });
    }
    Ok(bytes)
}

macro_rules! define_id {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; ID_BYTES]);

        impl $name {
            /// Textual prefix, also the type discriminator when parsing.
            pub const PREFIX: &'static str = $prefix;

            /// Mints a new identifier from the injected generator.
            pub fn generate(ids: &dyn IdGenerator) -> Self {
                Self(ids.next_bytes())
            }

            /// Materializes an identifier from a known payload (US-009 derives
            /// one for a v1 session from a hash).
            pub fn from_bytes(bytes: [u8; ID_BYTES]) -> Self {
                Self(bytes)
            }

            pub fn as_bytes(&self) -> &[u8; ID_BYTES] {
                &self.0
            }

            /// Parses the canonical textual form. Never panics.
            pub fn parse(raw: &str) -> Result<Self, IdError> {
                parse_payload(raw, Self::PREFIX).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", Self::PREFIX, hex::encode(self.0))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{self}")
            }
        }

        impl std::str::FromStr for $name {
            type Err = IdError;
            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                Self::parse(raw)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                Self::parse(&raw).map_err(D::Error::custom)
            }
        }
    };
}

define_id!(
    /// Owner of a conversation. Stable across resume, fork and restart.
    ThreadId,
    "thr_"
);
define_id!(
    /// One turn of a thread: exactly one terminal state per identifier.
    TurnId,
    "trn_"
);
define_id!(
    /// One model request inside a turn (US-006 captures its context).
    StepId,
    "stp_"
);
define_id!(
    /// One durable orchestration event.
    EventId,
    "evt_"
);
define_id!(
    /// One sub-agent in the parent-owned graph (EP-004).
    AgentId,
    "agt_"
);

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn random_ids_stay_unique_over_100k_generations() {
        let ids = RandomIds;
        let mut seen = HashSet::with_capacity(100_000);
        for _ in 0..100_000 {
            assert!(seen.insert(ThreadId::generate(&ids)), "duplicate ThreadId");
        }
        assert_eq!(seen.len(), 100_000);
    }

    #[test]
    fn an_injected_generator_is_reproducible_without_a_global_clock() {
        let first: Vec<String> = (0..8)
            .map(|_| TurnId::generate(&SequentialIds::new()).to_string())
            .collect();
        let shared = SequentialIds::new();
        let second: Vec<String> = (0..8)
            .map(|_| TurnId::generate(&shared).to_string())
            .collect();
        // Same seed, same values: the first vector rebuilds a generator each
        // time, so every entry is the first identifier of a fresh sequence.
        assert!(first.iter().all(|id| id == &first[0]));
        assert_eq!(second[0], first[0]);
        assert_eq!(second[1], "trn_00000000000000000000000000000002");
        assert!(second.iter().collect::<HashSet<_>>().len() == 8);
    }

    #[test]
    fn round_trips_through_json() {
        let id = ThreadId::generate(&SequentialIds::starting_at(42));
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"thr_0000000000000000000000000000002a\"");
        assert_eq!(serde_json::from_str::<ThreadId>(&json).unwrap(), id);
    }

    #[test]
    fn identifiers_are_ordered_and_comparable() {
        let ids = SequentialIds::new();
        let a = EventId::generate(&ids);
        let b = EventId::generate(&ids);
        assert!(a < b);
        assert_eq!(a, EventId::from_bytes(*a.as_bytes()));
    }

    #[test]
    fn rejects_empty_malformed_and_foreign_identifiers() {
        assert_eq!(ThreadId::parse("").unwrap_err(), IdError::Empty);
        assert!(matches!(
            ThreadId::parse("trn_00000000000000000000000000000001").unwrap_err(),
            IdError::Prefix {
                expected: "thr_",
                ..
            }
        ));
        assert!(matches!(
            ThreadId::parse("thr_00").unwrap_err(),
            IdError::Length { got: 2, .. }
        ));
        assert!(matches!(
            ThreadId::parse("thr_zz000000000000000000000000000000").unwrap_err(),
            IdError::Hex { .. }
        ));
        assert!(matches!(
            ThreadId::parse("thr_AA000000000000000000000000000000").unwrap_err(),
            IdError::Hex { .. }
        ));
    }

    #[test]
    fn deserialization_of_a_foreign_identifier_fails_without_panic() {
        let err = serde_json::from_str::<ThreadId>("\"agt_00000000000000000000000000000001\"")
            .expect_err("a foreign prefix must be refused");
        assert!(err.to_string().contains("thr_"));
        assert!(serde_json::from_str::<ThreadId>("42").is_err());
        assert!(serde_json::from_str::<ThreadId>("null").is_err());
    }
}
