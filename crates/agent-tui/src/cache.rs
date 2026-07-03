//! Cache des lignes stylées par bloc (US-041). Le rendu (`render.rs`) reconstruit
//! TOUT le transcript à chaque frame (modèle viewport + scroll interne, pas
//! d'`insert_before`). Sans cache, chaque frame re-parse le markdown ET re-colore
//! la syntaxe : coûteux et inutile (cf. opencode #811 : 25-30 % CPU idle sur un
//! re-render par timer). Ce cache mémoïse les `Vec<Line>` déjà « baked » par bloc
//! et ne laisse reconstruire que le bloc qui a changé (typiquement le dernier, en
//! cours de stream).
//!
//! Invalidation : une empreinte `u64` par bloc (contenu + `is_last`, qui pilote
//! l'aperçu du raisonnement en cours + l'appel apparié d'un résultat, dont dérivent
//! le résumé `⎿` et le diff) ; un garde au niveau cache sur `(largeur, truecolor)`
//! vide tout au resize (reflow) ou au changement de palette. `render` reste PUR :
//! le cache est en interior mutability (même patron que `scroll_max: Cell`), sans
//! aucune I/O et déterministe → toujours testable via `TestBackend`.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use ratatui::text::Line;
use serde_json::Value;

use crate::state::Block;

/// Une entrée de cache : l'empreinte du bloc tel que rendu + ses lignes stylées.
/// `fp == None` = slot vierge (jamais construit, ou invalidé par un resize).
#[derive(Clone, Default)]
struct Slot {
    fp: Option<u64>,
    lines: Vec<Line<'static>>,
}

/// Cache de rendu du transcript, aligné par index de bloc.
#[derive(Clone, Default)]
pub(crate) struct RenderCache {
    width: usize,
    truecolor: bool,
    ready: bool,
    slots: Vec<Slot>,
    /// Reconstructions de la dernière passe (instrumentation / tests) : 0 = tout
    /// servi depuis le cache.
    rebuilds: usize,
}

impl RenderCache {
    /// Prépare le cache pour une frame de `n` blocs à `(width, truecolor)` donnés :
    /// invalide tout si une dimension a changé (reflow / palette), aligne le nombre
    /// de slots sur `n`, et remet le compteur de reconstructions à 0.
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

    /// Lignes du bloc `i` : depuis le cache si l'empreinte `fp` correspond, sinon
    /// (re)construites par `build` puis mémoïsées. `begin` doit avoir dimensionné le
    /// cache à au moins `i + 1` slots.
    pub(crate) fn block_lines(
        &mut self,
        i: usize,
        fp: u64,
        build: impl FnOnce() -> Vec<Line<'static>>,
    ) -> &[Line<'static>] {
        debug_assert!(
            i < self.slots.len(),
            "begin() doit dimensionner le cache à >= i+1 slots avant block_lines"
        );
        let slot = &mut self.slots[i];
        if slot.fp != Some(fp) {
            slot.lines = build();
            slot.fp = Some(fp);
            self.rebuilds += 1;
        }
        &self.slots[i].lines
    }

    /// Nombre de blocs reconstruits lors de la dernière passe (0 = 100 % cache hit).
    pub(crate) fn rebuilds(&self) -> usize {
        self.rebuilds
    }
}

/// Empreinte d'un bloc tel qu'il sera rendu. Couvre tout ce que `push_block` lit :
/// le contenu du bloc, `is_last` (aperçu du raisonnement en cours), et — pour un
/// résultat d'outil — l'appel apparié (le résumé `⎿` et le diff inline en dérivent).
/// Un changement d'un seul de ces facteurs change l'empreinte → reconstruction.
pub(crate) fn fingerprint(
    block: &Block,
    is_last: bool,
    calls: &HashMap<&str, (&str, &Value)>,
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
            // L'aperçu des dernières lignes n'apparaît que sur le dernier bloc.
            is_last.hash(&mut h);
        }
        Block::ToolCall { name, input, .. } => {
            3u8.hash(&mut h);
            name.hash(&mut h);
            hash_value(input, &mut h);
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
            // Pas (encore) lu par `push_block`, mais inclus pour que l'invariant
            // « l'empreinte couvre tout l'état du bloc » survive à un futur badge.
            untrusted.hash(&mut h);
            // Le résumé `⎿` et le diff dérivent de l'appel apparié : un id orphelin
            // (résultat sans call) dégrade en empreinte sur l'id seul.
            match calls.get(call_id.as_str()) {
                Some((name, input)) => {
                    name.hash(&mut h);
                    hash_value(input, &mut h);
                }
                None => call_id.as_str().hash(&mut h),
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

/// Hash récursif d'un `serde_json::Value` SANS le sérialiser (évite une allocation
/// par frame). L'ordre des clés d'objet est déterministe (map interne ordonnée de
/// serde_json), donc l'empreinte est stable d'une frame à l'autre.
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

    fn calls() -> HashMap<&'static str, (&'static str, &'static Value)> {
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

        // Le texte change → empreinte différente.
        let c = Block::Assistant {
            text: "hello!".into(),
            streaming: false,
        };
        assert_ne!(
            fingerprint(&a, false, &calls()),
            fingerprint(&c, false, &calls())
        );

        // Le flag streaming compte (finalize_streaming doit invalider).
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
        let mut with_call: HashMap<&str, (&str, &Value)> = HashMap::new();
        with_call.insert("c1", ("edit", &input));
        let res = Block::ToolResult {
            call_id: "c1".into(),
            content: "ok".into(),
            untrusted: false,
            is_error: false,
            error_kind: None,
        };
        // Avec vs sans l'appel apparié → empreintes différentes (le diff en dépend).
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

        // 1re passe : tout est reconstruit.
        cache.begin(80, true, blocks.len());
        for (i, b) in blocks.iter().enumerate() {
            let fp = fingerprint(b, i == blocks.len() - 1, &calls());
            let _ = cache.block_lines(i, fp, || build(i));
        }
        assert_eq!(cache.rebuilds(), 2);

        // 2e passe identique : 0 reconstruction (100 % cache hit).
        cache.begin(80, true, blocks.len());
        for (i, b) in blocks.iter().enumerate() {
            let fp = fingerprint(b, i == blocks.len() - 1, &calls());
            let _ = cache.block_lines(i, fp, || build(i));
        }
        assert_eq!(cache.rebuilds(), 0);

        // Le dernier bloc change (token de stream) : une seule reconstruction.
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

        // Largeur différente → reflow → tout invalidé même à contenu identique.
        cache.begin(40, true, 1);
        let _ = cache.block_lines(0, fp, || vec![Line::from("x")]);
        assert_eq!(cache.rebuilds(), 1);

        // Perte du truecolor → palette différente → invalidation également.
        cache.begin(40, false, 1);
        let _ = cache.block_lines(0, fp, || vec![Line::from("x")]);
        assert_eq!(cache.rebuilds(), 1);
    }
}
