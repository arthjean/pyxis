//! Cache of styled lines per block (US-041). Rendering (`render.rs`) rebuilds
//! the WHOLE transcript on every frame (viewport model + internal scroll, no
//! `insert_before`). Without a cache, each frame re-parses the markdown AND re-colors
//! the syntax: expensive and pointless (see opencode #811: 25-30% idle CPU on a
//! timer-driven re-render). This cache memoizes the already "baked" `Vec<Line>` per block
//! and only lets the block that changed be rebuilt (typically the last one, being
//! streamed).
//!
//! Invalidation: one `u64` fingerprint per block (content + `is_last`, which drives
//! the preview of the reasoning in progress + the paired call of a result, from which
//! the `⎿` summary and the diff derive); a cache-level guard on `(width, truecolor)`
//! clears everything on resize (reflow) or on a palette change. `render` stays PURE:
//! the cache uses interior mutability (same pattern as `scroll_max: Cell`), without
//! any I/O and deterministic -> always testable through `TestBackend`.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use ratatui::text::Line;
use serde_json::Value;

use crate::state::Block;

/// A cache entry: the fingerprint of the block as rendered + its styled lines.
/// `fp == None` = blank slot (never built, or invalidated by a resize).
#[derive(Clone, Default)]
struct Slot {
    fp: Option<u64>,
    lines: Vec<Line<'static>>,
}

/// Transcript rendering cache, aligned by block index.
#[derive(Clone, Default)]
pub(crate) struct RenderCache {
    width: usize,
    truecolor: bool,
    ready: bool,
    slots: Vec<Slot>,
    /// Rebuilds of the last pass (instrumentation / tests): 0 = everything
    /// served from the cache.
    rebuilds: usize,
}

impl RenderCache {
    /// Prepares the cache for a frame of `n` blocks at the given `(width, truecolor)`:
    /// invalidates everything when a dimension changed (reflow / palette), aligns the number
    /// of slots on `n`, and resets the rebuild counter to 0.
    pub(crate) fn begin(&mut self, width: usize, truecolor: bool, n: usize) {
        if !self.ready || self.width != width || self.truecolor != truecolor {
            self.slots.clear();
            self.width = width;
            self.truecolor = truecolor;
            self.ready = true;
        }
        self.slots.resize_with(n, Slot::default);
        self.rebuilds = 0;
    }

    /// Lines of block `i`: from the cache when the `fp` fingerprint matches, otherwise
    /// (re)built by `build` then memoized. `begin` must have sized the
    /// cache to at least `i + 1` slots.
    pub(crate) fn block_lines(
        &mut self,
        i: usize,
        fp: u64,
        build: impl FnOnce() -> Vec<Line<'static>>,
    ) -> &[Line<'static>] {
        debug_assert!(
            i < self.slots.len(),
            "begin() should size the cache to >= i+1 slots before block_lines"
        );
        let slot = &mut self.slots[i];
        if slot.fp != Some(fp) {
            slot.lines = build();
            slot.fp = Some(fp);
            self.rebuilds += 1;
        }
        &self.slots[i].lines
    }

    /// Number of blocks rebuilt during the last pass (0 = 100% cache hit).
    pub(crate) fn rebuilds(&self) -> usize {
        self.rebuilds
    }
}

/// Fingerprint of a block as it will be rendered. Covers everything `push_block` reads:
/// the block content, `is_last` (preview of the reasoning in progress), and, for a
/// tool result, the paired call (the `⎿` summary and the inline diff derive from it).
/// A change in any single one of these factors changes the fingerprint -> rebuild.
pub(crate) fn fingerprint(
    block: &Block,
    is_last: bool,
    calls: &HashMap<&str, (&str, &Value, u64)>,
) -> u64 {
    let mut h = DefaultHasher::new();
    match block {
        Block::User(t) => {
            0u8.hash(&mut h);
            t.hash(&mut h);
        }
        Block::Assistant { text, streaming } => {
            1u8.hash(&mut h);
            text.hash(&mut h);
            streaming.hash(&mut h);
        }
        Block::Reasoning(t) => {
            2u8.hash(&mut h);
            t.hash(&mut h);
            // The preview of the last lines only appears on the last block.
            is_last.hash(&mut h);
        }
        Block::ToolCall {
            name, input_hash, ..
        } => {
            3u8.hash(&mut h);
            name.hash(&mut h);
            input_hash.hash(&mut h);
        }
        Block::ToolResult {
            call_id,
            content,
            is_error,
            untrusted,
            error_kind,
        } => {
            4u8.hash(&mut h);
            content.hash(&mut h);
            is_error.hash(&mut h);
            error_kind.hash(&mut h);
            // Not read by `push_block` (yet), but included so that the invariant
            // "the fingerprint covers the whole block state" survives a future badge.
            untrusted.hash(&mut h);
            // The `⎿` summary and the diff derive from the paired call: an orphan id
            // (result without a call) degrades to a fingerprint on the id alone.
            match calls.get(call_id.as_str()) {
                Some((name, _, input_hash)) => {
                    name.hash(&mut h);
                    input_hash.hash(&mut h);
                }
                None => call_id.as_str().hash(&mut h),
            }
        }
        Block::Plan(view) => {
            7u8.hash(&mut h);
            view.explanation.hash(&mut h);
            for step in &view.steps {
                step.step.hash(&mut h);
                // The enum has no `Hash`: its wire name is its identity here.
                crate::state::plan_status_label(step.status).hash(&mut h);
            }
        }
        Block::Notice(t) => {
            5u8.hash(&mut h);
            t.hash(&mut h);
        }
        Block::Error(t) => {
            6u8.hash(&mut h);
            t.hash(&mut h);
        }
    }
    h.finish()
}

pub(crate) fn value_hash(v: &Value) -> u64 {
    let mut h = DefaultHasher::new();
    hash_value(v, &mut h);
    h.finish()
}

/// Recursive hash of a `serde_json::Value` WITHOUT serializing it (avoids one allocation
/// per frame). The order of object keys is deterministic (ordered internal map of
/// serde_json), so the fingerprint is stable from one frame to the next.
fn hash_value(v: &Value, h: &mut impl Hasher) {
    match v {
        Value::Null => 0u8.hash(h),
        Value::Bool(b) => {
            1u8.hash(h);
            b.hash(h);
        }
        Value::Number(n) => {
            2u8.hash(h);
            n.to_string().hash(h);
        }
        Value::String(s) => {
            3u8.hash(h);
            s.hash(h);
        }
        Value::Array(a) => {
            4u8.hash(h);
            for it in a {
                hash_value(it, h);
            }
        }
        Value::Object(o) => {
            5u8.hash(h);
            for (k, val) in o {
                k.hash(h);
                hash_value(val, h);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Line;
    use serde_json::json;

    fn calls() -> HashMap<&'static str, (&'static str, &'static Value, u64)> {
        HashMap::new()
    }

    #[test]
    fn fingerprint_is_stable_and_content_sensitive() {
        let a = Block::Assistant {
            text: "hello".into(),
            streaming: false,
        };
        let b = Block::Assistant {
            text: "hello".into(),
            streaming: false,
        };
        assert_eq!(
            fingerprint(&a, false, &calls()),
            fingerprint(&b, false, &calls())
        );

        // The text changes -> different fingerprint.
        let c = Block::Assistant {
            text: "hello!".into(),
            streaming: false,
        };
        assert_ne!(
            fingerprint(&a, false, &calls()),
            fingerprint(&c, false, &calls())
        );

        // The streaming flag counts (finalize_streaming must invalidate).
        let d = Block::Assistant {
            text: "hello".into(),
            streaming: true,
        };
        assert_ne!(
            fingerprint(&a, false, &calls()),
            fingerprint(&d, false, &calls())
        );
    }

    #[test]
    fn reasoning_fingerprint_depends_on_is_last() {
        let r = Block::Reasoning("thinking".into());
        assert_ne!(
            fingerprint(&r, true, &calls()),
            fingerprint(&r, false, &calls()),
            "l'aperçu du raisonnement en cours ne s'affiche que sur le dernier bloc"
        );
    }

    #[test]
    fn tool_result_fingerprint_tracks_paired_call() {
        let input = json!({"path": "a.rs", "old_string": "x", "new_string": "y"});
        let mut with_call: HashMap<&str, (&str, &Value, u64)> = HashMap::new();
        with_call.insert("c1", ("edit", &input, value_hash(&input)));
        let res = Block::ToolResult {
            call_id: "c1".into(),
            content: "ok".into(),
            untrusted: false,
            is_error: false,
            error_kind: None,
        };
        // With vs without the paired call -> different fingerprints (the diff depends on it).
        assert_ne!(
            fingerprint(&res, false, &with_call),
            fingerprint(&res, false, &calls())
        );
    }

    #[test]
    fn cache_serves_unchanged_blocks_and_rebuilds_only_the_changed_one() {
        let mut cache = RenderCache::default();
        let blocks = [
            Block::User("hi".into()),
            Block::Assistant {
                text: "world".into(),
                streaming: true,
            },
        ];
        let build = |_i: usize| vec![Line::from("x")];

        // 1st pass: everything is rebuilt.
        cache.begin(80, true, blocks.len());
        for (i, b) in blocks.iter().enumerate() {
            let fp = fingerprint(b, i == blocks.len() - 1, &calls());
            let _ = cache.block_lines(i, fp, || build(i));
        }
        assert_eq!(cache.rebuilds(), 2);

        // Identical 2nd pass: 0 rebuild (100% cache hit).
        cache.begin(80, true, blocks.len());
        for (i, b) in blocks.iter().enumerate() {
            let fp = fingerprint(b, i == blocks.len() - 1, &calls());
            let _ = cache.block_lines(i, fp, || build(i));
        }
        assert_eq!(cache.rebuilds(), 0);

        // The last block changes (stream token): a single rebuild.
        let blocks2 = [
            Block::User("hi".into()),
            Block::Assistant {
                text: "world!".into(),
                streaming: true,
            },
        ];
        cache.begin(80, true, blocks2.len());
        for (i, b) in blocks2.iter().enumerate() {
            let fp = fingerprint(b, i == blocks2.len() - 1, &calls());
            let _ = cache.block_lines(i, fp, || build(i));
        }
        assert_eq!(cache.rebuilds(), 1);
    }

    #[test]
    fn resize_invalidates_whole_cache() {
        let mut cache = RenderCache::default();
        let block = Block::User("hi".into());
        cache.begin(80, true, 1);
        let fp = fingerprint(&block, false, &calls());
        let _ = cache.block_lines(0, fp, || vec![Line::from("x")]);
        assert_eq!(cache.rebuilds(), 1);

        // Different width -> reflow -> everything invalidated even with identical content.
        cache.begin(40, true, 1);
        let _ = cache.block_lines(0, fp, || vec![Line::from("x")]);
        assert_eq!(cache.rebuilds(), 1);

        // Loss of truecolor -> different palette -> invalidation as well.
        cache.begin(40, false, 1);
        let _ = cache.block_lines(0, fp, || vec![Line::from("x")]);
        assert_eq!(cache.rebuilds(), 1);
    }
}
