//! Spill policy (US-073/US-074/US-075): what the model reads INSTEAD of a tool
//! output too large to be returned whole.
//!
//! The storage ([`crate::spill`]) decides nothing and this module stores
//! nothing. The split is the reference's own (`spill` against `spill-policy`):
//! a policy able to write too would make "best effort" untestable, since the
//! failure it has to absorb is exactly the write's.
//!
//! Best effort is strict. No storage, a failed write, or a notice that does not
//! itself fit under the cap: each logs at warn level and keeps the ORIGINAL
//! result, byte for byte. A spill failure never turns a successful call into an
//! error and never hides content.
//!
//! WHEN a spill happens is decided by the caller, in `run_one_inner`, the one
//! place where the full text still exists and the call context is known. This
//! module only answers "given that this output must be spilled, what replaces
//! it".

use agent_core::tools::{ToolResultTruncation, TruncationStrategy};

use crate::spill::SpillStore;
use crate::tool::MAX_TOOL_OUTPUT_BYTES;

/// Tools whose output is never spilled.
///
/// `read` is excluded to break the loop it would otherwise close: the model
/// reads a file, the policy spills the read back to a file, and the notice
/// invites the model to read that file, which spills again. A tool whose whole
/// purpose is to page through a file already offers `offset` and `limit`, which
/// is the recovery the notice would have suggested anyway.
pub const NEVER_SPILLED: &[&str] = &["read"];

/// The two bytes joining the preview to its notice, reserved along with it.
const JOIN: &str = "\n\n";

/// A bounded replacement and the truncation record that describes it.
pub struct Spilled {
    /// Head preview, tail preview, then the notice carrying the locator.
    pub content: String,
    /// `original_bytes` is the size of the FULL output, not of the preview:
    /// the point of the record is to say how much was set aside.
    pub truncation: ToolResultTruncation,
}

/// Writes `content` whole and builds the bounded replacement, or returns `None`
/// when the original must be kept.
///
/// `None` is never an error: every one of its causes is a degradation the
/// caller absorbs by leaving the result exactly as it was.
pub fn replace_with_spill(
    store: &SpillStore,
    tool_name: &str,
    call_id: &str,
    content: &str,
) -> Option<Spilled> {
    let saved = match store.save(tool_name, call_id, content) {
        Ok(saved) => saved,
        Err(error) => {
            // Permissions, ENOSPC, a taken name: none of them is the model's
            // problem, and none of them may cost it the output it asked for.
            tracing::warn!(
                target: "pyxis::tools",
                tool = %tool_name,
                error = %error,
                "spill write failed; keeping the original result"
            );
            return None;
        }
    };

    // The notice's byte cost is reserved INSIDE the cap before the preview is
    // cut: a preview spending the whole budget and then appending the notice
    // would exceed the cap, and for a marginally oversized output would exceed
    // the ORIGINAL. The reservation prices the notice at the worst case
    // omission (the whole output), whose digit count bounds the real one's, so
    // the final notice is never longer than what was reserved.
    let reserve = notice(content.len(), &saved.locator).len() + JOIN.len();
    let budget = MAX_TOOL_OUTPUT_BYTES.saturating_sub(reserve);
    let (head, tail) = preview(content, budget);
    let omitted = content.len() - head.len() - tail.len();
    let notice = notice(omitted, &saved.locator);
    let replacement = if head.is_empty() && tail.is_empty() {
        notice
    } else {
        format!("{head}{tail}{JOIN}{notice}")
    };
    if replacement.len() > MAX_TOOL_OUTPUT_BYTES {
        // No replacement fits: the notice alone is longer than the cap. The
        // file already written stays behind as a harmless orphan; US-081 owns
        // the directory bound, and deleting it here would trade a bounded
        // leftover for a second failure path on the same call.
        tracing::warn!(
            target: "pyxis::tools",
            tool = %tool_name,
            "spill notice exceeds the tool output cap; keeping the original result"
        );
        return None;
    }
    Some(Spilled {
        truncation: ToolResultTruncation {
            original_bytes: content.len(),
            kept_bytes: replacement.len(),
            // The enum has no head-tail variant and this change does not add
            // one: the preview starts at the head, and the notice states the
            // exact shape in the one place the model reads.
            strategy: TruncationStrategy::Head,
            // The locator travels bare, as an opaque handle: a consumer
            // displays it, no consumer parses it.
            continuation_hint: saved.locator,
        },
        content: replacement,
    })
}

/// The one line the model reads about its own missing output. English, like
/// everything addressed to the model, and parenthesized on a single line so it
/// cannot be mistaken for a continuation of the preview.
fn notice(omitted: usize, locator: &str) -> String {
    format!(
        "(Omitted {omitted} bytes from the middle. Full output saved to {locator}. \
         Read it with `read` using `offset` and `limit`, or search it with `grep`.)"
    )
}

/// Splits `budget` bytes between the two ends of `text`, the upper half going
/// to the head.
///
/// Both cuts land on a character boundary, so a multi-byte character straddling
/// either limit is dropped rather than halved: the two slices are always valid
/// UTF-8 and are never joined across a character.
///
/// The caller only calls this when `text` is longer than the cap, hence longer
/// than `budget`, so the head and the tail can never overlap.
fn preview(text: &str, budget: usize) -> (&str, &str) {
    let head_end = floor_boundary(text, budget.div_ceil(2));
    let tail_start = ceil_boundary(text, text.len().saturating_sub(budget / 2).max(head_end));
    (&text[..head_end], &text[tail_start..])
}

fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spill::SpillStore;
    use crate::tool::Tool as _;
    use std::path::{Path, PathBuf};

    struct Workspace(PathBuf);

    impl Workspace {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("pyxis-policy-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The exclusion list names a tool that exists: a rename would otherwise
    /// silently re-open the read -> spill -> read loop.
    #[test]
    fn the_excluded_tool_name_is_the_read_tool_name() {
        assert!(NEVER_SPILLED.contains(&crate::read::Read.name()));
    }

    /// US-074 AC3, on the size the PRD measures.
    #[test]
    fn a_ten_mebibyte_output_is_replaced_by_something_that_fits_the_cap() {
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        let content: String = std::iter::repeat_n("line\n", 10 * 1024 * 1024 / 5).collect();

        let spilled = replace_with_spill(&store, "bash", "call_1", &content).unwrap();

        assert!(
            spilled.content.len() <= MAX_TOOL_OUTPUT_BYTES,
            "preview plus notice must fit the cap, got {}",
            spilled.content.len()
        );
        assert_eq!(spilled.truncation.original_bytes, content.len());
        assert_eq!(spilled.truncation.kept_bytes, spilled.content.len());
        // The file holds everything the replacement no longer does.
        let on_disk = std::fs::metadata(ws.path().join(&spilled.truncation.continuation_hint))
            .unwrap()
            .len();
        assert_eq!(on_disk, content.len() as u64);
    }

    /// US-074 AC1: both ends survive, and the accounting closes.
    #[test]
    fn the_replacement_shows_both_ends_and_counts_what_it_dropped() {
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        let content = format!("HEAD{}TAIL", "x".repeat(MAX_TOOL_OUTPUT_BYTES));

        let spilled = replace_with_spill(&store, "bash", "call_1", &content).unwrap();

        assert!(spilled.content.starts_with("HEAD"), "{}", &spilled.content);
        let (preview, notice) = spilled.content.split_once(JOIN).unwrap();
        assert!(preview.ends_with("TAIL"), "tail lost");
        let omitted = content.len() - preview.len();
        assert!(
            notice.contains(&format!("Omitted {omitted} bytes")),
            "{notice}"
        );
        assert!(
            notice.contains(&spilled.truncation.continuation_hint),
            "{notice}"
        );
    }

    /// US-074 AC2: the reservation is priced at the worst case, so the notice
    /// never grows past it whatever the real omission count turns out to be.
    #[test]
    fn the_notice_never_exceeds_what_was_reserved_for_it() {
        let locator = ".pyxis/spill/0123456789ab/aabbccddeeff-bash-call_1.txt";
        for total in [30_001usize, 999_999, 10 * 1024 * 1024, usize::MAX / 2] {
            let reserved = notice(total, locator).len();
            for omitted in [0usize, 1, 30_000, total] {
                assert!(
                    notice(omitted, locator).len() <= reserved,
                    "omitted={omitted} priced above the worst case {total}"
                );
            }
        }
    }

    /// US-074 unhappy path: a multi-byte character sitting exactly on the head
    /// limit is dropped whole, never halved.
    #[test]
    fn a_character_straddling_the_head_limit_leaves_valid_utf8() {
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        // The budget is the cap minus the reserved notice; the head takes its
        // upper half. Build the text so a 4-byte character starts one byte
        // before that limit whatever the locator length turns out to be.
        let probe = store.save("probe", "call_0", "x").unwrap();
        let reserve = notice(0, &probe.locator).len() + JOIN.len();
        let head_limit = MAX_TOOL_OUTPUT_BYTES.saturating_sub(reserve).div_ceil(2);
        let mut content = "a".repeat(head_limit - 1);
        content.push('\u{1F600}');
        content.push_str(&"b".repeat(MAX_TOOL_OUTPUT_BYTES));

        let spilled = replace_with_spill(&store, "bash", "call_1", &content).unwrap();

        // Valid UTF-8 by construction (it is a `String`), and the straddling
        // character is absent rather than cut in half.
        assert!(!spilled.content.contains('\u{1F600}'));
        let (preview, _) = spilled.content.split_once(JOIN).unwrap();
        assert!(preview.starts_with(&"a".repeat(head_limit - 1)));
        assert!(preview.ends_with('b'));
    }

    /// The gap is real: head and tail never touch, so no character is ever
    /// rebuilt across the omission.
    #[test]
    fn the_head_and_the_tail_never_overlap() {
        let text = "é".repeat(1_000);
        let (head, tail) = preview(&text, 101);
        assert!(head.len() <= 51 && tail.len() <= 50);
        assert!(head.len() + tail.len() < text.len());
        assert!(text.starts_with(head) && text.ends_with(tail));
    }

    /// US-073 AC5 and its unhappy path, at the policy level: a write failure
    /// yields nothing to replace with, so the caller keeps the original.
    #[test]
    fn a_write_failure_yields_no_replacement_at_all() {
        use std::os::unix::fs::PermissionsExt as _;
        let ws = Workspace::new();
        let store = SpillStore::create(ws.path(), "thread_1").unwrap();
        let mut perms = std::fs::metadata(store.root()).unwrap().permissions();
        perms.set_mode(0o500);
        std::fs::set_permissions(store.root(), perms).unwrap();

        let content = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 1);
        assert!(replace_with_spill(&store, "bash", "call_1", &content).is_none());

        let mut perms = std::fs::metadata(store.root()).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(store.root(), perms).unwrap();
    }
}
