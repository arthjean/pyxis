//! Composing the name a server's tool is exposed under.
//!
//! The model API accepts `^[A-Za-z0-9_-]+$` over at most 64 bytes.
//! `mcp__{server}__{tool}` overflows as soon as both names are long, so the
//! composed name is sanitized, then shortened deterministically with a
//! fingerprint of the pair. Uniqueness is a property of the whole registered
//! set, not of one server, which is why the caller carries the `taken` set
//! across servers.

use std::collections::BTreeSet;

/// Prefix of every registered MCP tool. Its presence makes a collision with a
/// native tool impossible.
pub const NAME_PREFIX: &str = "mcp__";
/// Name cap imposed by the model API (`ToolSpec::validate`).
pub const MAX_NAME_BYTES: usize = 64;

/// Composes the exposed name: `mcp__{server}__{tool}`, sanitized, shortened
/// deterministically when it overflows, and unique within `taken`.
pub fn qualified_name(server: &str, tool: &str, taken: &BTreeSet<String>) -> String {
    let base = format!(
        "{NAME_PREFIX}{}__{}",
        sanitize_part(server),
        sanitize_part(tool)
    );
    let fingerprint = fingerprint(server, tool);
    // Bounded by construction: `taken` is finite, so a free name is reached in at
    // most `taken.len() + 1` attempts.
    let mut attempt = 0_usize;
    loop {
        let candidate = if attempt == 0 && base.len() <= MAX_NAME_BYTES {
            base.clone()
        } else if attempt == 0 {
            shorten(&base, &format!("_{fingerprint}"))
        } else {
            shorten(&base, &format!("_{fingerprint}_{attempt}"))
        };
        if !taken.contains(&candidate) {
            return candidate;
        }
        attempt += 1;
    }
}

/// Keeps only what the model API accepts; anything else becomes `_`. The result
/// is pure ASCII, hence safe to cut on a byte boundary.
fn sanitize_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "_".to_string()
    } else {
        sanitized
    }
}

/// Cuts `base` so that `base_prefix + suffix` fits in `MAX_NAME_BYTES`. `base` is
/// ASCII (post-sanitize), so a byte cut is a character cut.
fn shorten(base: &str, suffix: &str) -> String {
    let keep = MAX_NAME_BYTES.saturating_sub(suffix.len());
    let mut out: String = base.chars().take(keep).collect();
    out.push_str(suffix);
    out
}

/// FNV-1a over `server\0tool`. Hand-rolled on purpose: `DefaultHasher` is not
/// stable across Rust versions, and "deterministic" is the property the shortened
/// name depends on.
fn fingerprint(server: &str, tool: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in server
        .as_bytes()
        .iter()
        .chain(std::iter::once(&0_u8))
        .chain(tool.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", (hash >> 32) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_prefixed_by_its_server() {
        let taken = BTreeSet::new();
        assert_eq!(qualified_name("files", "read", &taken), "mcp__files__read");
    }

    #[test]
    fn same_tool_on_two_servers_gets_two_names() {
        let mut taken = BTreeSet::new();
        let a = qualified_name("alpha", "search", &taken);
        taken.insert(a.clone());
        let b = qualified_name("beta", "search", &taken);
        assert_ne!(a, b);
        assert_eq!(a, "mcp__alpha__search");
        assert_eq!(b, "mcp__beta__search");
    }

    #[test]
    fn long_names_are_shortened_deterministically_and_stay_unique() {
        let server = "a".repeat(60);
        let taken = BTreeSet::new();
        let first = qualified_name(&server, &"b".repeat(60), &taken);
        let second = qualified_name(&server, &"b".repeat(61), &taken);
        assert!(first.len() <= MAX_NAME_BYTES, "{} bytes", first.len());
        assert!(second.len() <= MAX_NAME_BYTES);
        // Same shared prefix, different fingerprint -> no silent collision.
        assert_ne!(first, second);
        assert_eq!(first, qualified_name(&server, &"b".repeat(60), &taken));
    }

    #[test]
    fn a_taken_name_forces_a_distinct_candidate() {
        let mut taken = BTreeSet::new();
        taken.insert("mcp__files__read".to_string());
        let name = qualified_name("files", "read", &taken);
        assert_ne!(name, "mcp__files__read");
        assert!(name.starts_with("mcp__files__read"));
        assert!(name.len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn out_of_charset_characters_are_sanitized() {
        let taken = BTreeSet::new();
        let name = qualified_name("my server", "tool.v2:beta", &taken);
        assert_eq!(name, "mcp__my_server__tool_v2_beta");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
    }

    #[test]
    fn sanitizing_does_not_create_a_silent_collision() {
        let mut taken = BTreeSet::new();
        let a = qualified_name("srv", "tool.v2", &taken);
        taken.insert(a.clone());
        let b = qualified_name("srv", "tool:v2", &taken);
        assert_ne!(a, b, "two distinct tools must keep two names");
    }
}
