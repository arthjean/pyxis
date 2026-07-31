//! Length caps on server-authored text. One place, because every one of these
//! strings comes from a remote party and the rule is the same: bound it at the
//! boundary, not at the point of use.

/// Length cap of a tool description (ARCHITECTURE 6: a server cannot pollute the
/// prompt).
pub(crate) const DESCRIPTION_CAP: usize = 2048;
/// Length cap of the server instructions. Tighter than a description because the
/// text is prose about the server rather than about one tool, and because it is
/// only ever shown to the human (`/mcp <server> info`), never to the model.
pub const INSTRUCTIONS_CAP: usize = 512;

/// Truncates `s` to `max` chars (never in the middle of a multi-byte char).
pub(crate) fn cap(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capping_counts_chars_and_never_splits_one() {
        assert_eq!(cap("héllo", 2), "hé");
        assert_eq!(cap("short", 64), "short");
        assert_eq!(
            cap(&"x".repeat(INSTRUCTIONS_CAP * 2), INSTRUCTIONS_CAP).len(),
            INSTRUCTIONS_CAP
        );
    }
}
