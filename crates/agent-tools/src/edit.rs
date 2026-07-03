//! Outil `edit` — remplacement ancré tolérant (US-025). `old_string` est localisé
//! par 4 passes successives : exact (sous-chaîne, édition intra-ligne) → `trim_end`
//! → `trim` → normalisation Unicode (lignes), pour absorber les divergences que
//! GPT-5.x génère de mémoire (NBSP, tirets/guillemets typographiques). La PREMIÈRE
//! passe à correspondance UNIQUE gagne ; ≥ 2 correspondances → ambiguë, 0 après les
//! 4 passes → introuvable (échec explicite, AUCUNE mutation, edge case #11). Le
//! remplacement s'applique aux lignes ORIGINALES (contenu hors-cible intact).
//! Mutation confinée au workspace.

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{ToolError, ValidationError};
use crate::path::confine;
use crate::permission::{PermCtx, PermissionDecision};
use crate::tool::{Tool, ToolCtx, ToolOutput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditInput {
    pub path: String,
    /// Texte à remplacer — doit être unique dans le fichier.
    pub old_string: String,
    pub new_string: String,
}

pub struct Edit;

#[async_trait]
impl Tool for Edit {
    type Input = EditInput;

    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> String {
        "Remplace une occurrence unique de texte dans un fichier. old_string doit \
         localiser une cible unique (sinon l'édition échoue sans rien modifier). \
         La localisation tolère les divergences d'espaces en fin de ligne et de \
         caractères Unicode (tirets/guillemets typographiques, NBSP). Paramètres : \
         path, old_string, new_string."
            .to_string()
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Chemin du fichier (relatif au workspace)." },
                "old_string": { "type": "string", "description": "Texte à remplacer (ancre unique)." },
                "new_string": { "type": "string", "description": "Texte de remplacement." }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_sensitive(&self) -> bool {
        false
    }
    fn returns_untrusted(&self) -> bool {
        false
    }
    fn behavioral_guidelines(&self) -> &[&'static str] {
        EDIT_GUIDELINES
    }
    fn validate_input(&self, input: &Self::Input) -> Result<(), ValidationError> {
        if input.old_string.is_empty() {
            return Err(ValidationError::new(
                "old_string vide : impossible d'ancrer l'édition",
            ));
        }
        if input.old_string == input.new_string {
            return Err(ValidationError::new(
                "old_string == new_string : édition sans effet",
            ));
        }
        Ok(())
    }
    fn permission(&self, _input: &Self::Input, _ctx: &PermCtx) -> PermissionDecision {
        PermissionDecision::Ask
    }

    async fn call(&self, input: Self::Input, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let path = confine(&ctx.workspace, &input.path)?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", input.path)))?;

        let (updated, level) = match locate(&content, &input.old_string) {
            Ok(Anchor::Substring) => (
                content.replacen(&input.old_string, &input.new_string, 1),
                MatchLevel::Exact,
            ),
            Ok(Anchor::Lines { start, len, level }) => (
                apply_line_window(&content, start, len, &input.new_string),
                level,
            ),
            Err(LocateError::Ambiguous { count }) => {
                return Err(ToolError::Rejected(format!(
                    "ancre ambiguë dans {} : {} correspondances — ajoutez du contexte \
                     unique autour de old_string",
                    input.path, count
                )));
            }
            Err(LocateError::NotFound) => {
                return Err(ToolError::Rejected(format!(
                    "ancre introuvable dans {} après 4 passes (exact, trim_end, trim, \
                     Unicode) — vérifiez old_string ou ajoutez du contexte",
                    input.path
                )));
            }
        };

        tokio::fs::write(&path, updated.as_bytes())
            .await
            .map_err(|e| ToolError::Io(format!("{}: {e}", input.path)))?;
        Ok(ToolOutput::text(format!(
            "Édité : {} ({})",
            input.path,
            level.label()
        )))
    }
}

/// Invariant comportemental co-localisé avec l'outil (US-026), collecté par le
/// Registry et injecté dans le system prompt.
const EDIT_GUIDELINES: &[&str] = &[
    "edit : old_string est cherché dans le contenu ACTUEL du fichier sur disque, \
     pas après tes autres edits du même tour ; ré-ancre chaque edit sur l'état \
     courant et inclus assez de contexte pour une ancre unique.",
];

/// Niveau de passe de localisation atteint (observabilité, US-025 AC4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchLevel {
    Exact,
    TrimEnd,
    Trim,
    Unicode,
}

impl MatchLevel {
    fn label(self) -> &'static str {
        match self {
            MatchLevel::Exact => "niveau 1 : exact",
            MatchLevel::TrimEnd => "niveau 2 : trim_end",
            MatchLevel::Trim => "niveau 3 : trim",
            MatchLevel::Unicode => "niveau 4 : normalisation Unicode",
        }
    }
}

/// Localisation réussie de l'ancre.
enum Anchor {
    /// Match exact en sous-chaîne (préserve l'édition intra-ligne, offsets exacts).
    Substring,
    /// Fenêtre de lignes `[start, start+len)` trouvée par une passe fuzzy.
    Lines {
        start: usize,
        len: usize,
        level: MatchLevel,
    },
}

enum LocateError {
    /// ≥ 2 correspondances à une passe (ou en exact) → ambiguïté irréductible
    /// (les passes plus permissives ne feraient que fusionner davantage, jamais
    /// distinguer). Pas de niveau : il ne renseigne que le SUCCÈS (AC4).
    Ambiguous { count: usize },
    /// Aucune correspondance après les 4 passes.
    NotFound,
}

/// Localise `old` dans `content` par 4 passes (exact → trim_end → trim → Unicode).
/// La passe exacte reste en SOUS-CHAÎNE (édition intra-ligne préservée) ; les passes
/// fuzzy sont LIGNE À LIGNE pour appliquer le remplacement sur les lignes ORIGINALES.
/// Résolution déterministe : la première passe à correspondance UNIQUE gagne ;
/// sinon ≥ 2 → ambiguë, 0 partout → introuvable.
fn locate(content: &str, old: &str) -> Result<Anchor, LocateError> {
    // Passe 1 : exact sous-chaîne.
    let exact = content.matches(old).count();
    if exact == 1 {
        return Ok(Anchor::Substring);
    }
    // Passes 2-4 : ligne à ligne, normalisation croissante.
    let content_lines: Vec<&str> = content.split('\n').collect();
    let old_lines: Vec<&str> = old.split('\n').collect();
    for level in [MatchLevel::TrimEnd, MatchLevel::Trim, MatchLevel::Unicode] {
        let windows = find_windows(&content_lines, &old_lines, level);
        match windows.len() {
            0 => continue,
            1 => {
                return Ok(Anchor::Lines {
                    start: windows[0],
                    len: old_lines.len(),
                    level,
                });
            }
            n => return Err(LocateError::Ambiguous { count: n }),
        }
    }
    if exact >= 2 {
        Err(LocateError::Ambiguous { count: exact })
    } else {
        Err(LocateError::NotFound)
    }
}

/// Indices de départ des fenêtres de `content_lines` qui matchent `old_lines` sous
/// la normalisation de `level`.
fn find_windows(content_lines: &[&str], old_lines: &[&str], level: MatchLevel) -> Vec<usize> {
    if old_lines.is_empty() || old_lines.len() > content_lines.len() {
        return Vec::new();
    }
    // Normalisation pré-calculée UNE fois par ligne (sinon chaque ligne du fichier
    // était re-normalisée jusqu'à `old_lines.len()` fois → O(n×m) allocations).
    let content_n: Vec<String> = content_lines.iter().map(|l| norm(l, level)).collect();
    let pat: Vec<String> = old_lines.iter().map(|l| norm(l, level)).collect();
    let last = content_lines.len() - old_lines.len();
    let mut out = Vec::new();
    for i in 0..=last {
        if content_n[i..i + pat.len()] == pat[..] {
            out.push(i);
        }
    }
    out
}

/// Normalise une ligne selon le niveau de passe (croissant en permissivité).
fn norm(line: &str, level: MatchLevel) -> String {
    match level {
        MatchLevel::Exact | MatchLevel::TrimEnd => line.trim_end().to_string(),
        MatchLevel::Trim => line.trim().to_string(),
        MatchLevel::Unicode => normalize_unicode_line(line),
    }
}

/// Table de normalisation Unicode (US-025 AC5), reprise de Pi/Codex CLI : dashes
/// U+2010-U+2015 & U+2212 → `-`, quotes typographiques → ASCII, NBSP & espaces
/// spéciales → ` `. `trim` inclus (passe la plus permissive). Pas de NFKC (table de
/// caractères explicite, sans dépendance externe).
fn normalize_unicode_line(line: &str) -> String {
    let mapped: String = line
        .chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect();
    mapped.trim().to_string()
}

/// Remplace la fenêtre de lignes ORIGINALES `[start, start+len)` par `new`, en
/// rejoignant le reste byte-pour-byte (contenu hors-cible intact). Préserve le
/// terminateur dominant : sur un fichier CRLF, les segments hors-cible gardent leur
/// `\r` ; on aligne les lignes de `new` dessus (sinon la passe fuzzy, qui matche en
/// strippant `\r`, réinjecterait `new` en LF → fins de ligne MIXTES dans la région
/// éditée).
fn apply_line_window(content: &str, start: usize, len: usize, new: &str) -> String {
    let segs: Vec<&str> = content.split('\n').collect();
    let crlf = content.contains("\r\n");
    let mut result: Vec<String> = Vec::with_capacity(segs.len());
    result.extend(segs[..start].iter().map(|s| (*s).to_string()));
    for nl in new.split('\n') {
        // normalise les fins de ligne de `new` sur celles du fichier (strip puis
        // réattache `\r` si CRLF) → région éditée cohérente.
        let core = nl.strip_suffix('\r').unwrap_or(nl);
        result.push(if crlf {
            format!("{core}\r")
        } else {
            core.to_string()
        });
    }
    result.extend(segs[start + len..].iter().map(|s| (*s).to_string()));
    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_substring_is_level_1_and_intramline() {
        // « UNIQUE » intra-ligne → match exact sous-chaîne (offsets préservés).
        assert!(matches!(
            locate("alpha UNIQUE beta\n", "UNIQUE"),
            Ok(Anchor::Substring)
        ));
    }

    #[test]
    fn trim_end_pass_resolves_trailing_whitespace() {
        // Ancre MULTI-ligne dont une ligne du fichier porte un espace final absent
        // de l'ancre : la sous-chaîne exacte échoue, trim_end ligne-à-ligne résout.
        let content = "foo \nbar\nbaz\n";
        assert!(matches!(
            locate(content, "foo\nbar"),
            Ok(Anchor::Lines {
                start: 0,
                len: 2,
                level: MatchLevel::TrimEnd
            })
        ));
    }

    #[test]
    fn trim_pass_resolves_leading_whitespace() {
        // Différence d'espace EN TÊTE de ligne : trim_end ne suffit pas, trim oui.
        let content = "  foo \nbar\nbaz\n";
        assert!(matches!(
            locate(content, "foo\nbar"),
            Ok(Anchor::Lines {
                start: 0,
                len: 2,
                level: MatchLevel::Trim
            })
        ));
    }

    #[test]
    fn multi_line_exact_substring_is_level_1() {
        // Ancre multi-ligne PRÉSENTE telle quelle → passe exacte (sous-chaîne).
        assert!(matches!(
            locate("x\nfoo\nbar\ny\n", "foo\nbar"),
            Ok(Anchor::Substring)
        ));
    }

    #[test]
    fn unicode_pass_absorbs_nbsp_dash_and_quotes() {
        // fichier : NBSP + em-dash + smart quotes ; ancre : ASCII.
        let content = "let s = \u{201C}h\u{00A0}\u{2014}llo\u{201D};\nx\n";
        let r = locate(content, "let s = \"h -llo\";");
        assert!(
            matches!(
                r,
                Ok(Anchor::Lines {
                    level: MatchLevel::Unicode,
                    start: 0,
                    len: 1
                })
            ),
            "la passe Unicode doit absorber NBSP/dash/quotes"
        );
    }

    #[test]
    fn ambiguous_two_lines_rejected_without_mutation() {
        let r = locate("dup\ndup\n", "dup");
        assert!(matches!(r, Err(LocateError::Ambiguous { .. })));
    }

    #[test]
    fn not_found_after_all_passes() {
        assert!(matches!(
            locate("alpha\nbeta\n", "gamma"),
            Err(LocateError::NotFound)
        ));
    }

    #[test]
    fn apply_window_keeps_surrounding_bytes_intact() {
        // ancre multi-ligne localisée par trim_end (espace final cassant le substring).
        let content = "a\nfoo \nbar\nz\n";
        match locate(content, "foo\nbar") {
            Ok(Anchor::Lines { start, len, .. }) => {
                assert_eq!((start, len), (1, 2));
                let out = apply_line_window(content, start, len, "NEW");
                assert_eq!(out, "a\nNEW\nz\n", "lignes hors-cible intactes");
            }
            _ => unreachable!("localisation par fenêtre attendue"),
        }
    }

    #[test]
    fn apply_window_preserves_crlf_endings() {
        // fichier CRLF : la passe fuzzy matche (trim_end strippe \r) mais le
        // remplacement doit rester en CRLF, pas introduire de fins de ligne mixtes.
        let content = "a\r\nfoo \r\nbar\r\nz\r\n";
        match locate(content, "foo\nbar") {
            Ok(Anchor::Lines { start, len, .. }) => {
                let out = apply_line_window(content, start, len, "NEW1\nNEW2");
                assert_eq!(out, "a\r\nNEW1\r\nNEW2\r\nz\r\n", "CRLF préservé partout");
            }
            _ => unreachable!("localisation par fenêtre attendue"),
        }
    }

    #[test]
    fn unicode_table_covers_required_chars() {
        assert_eq!(normalize_unicode_line("\u{2010}\u{2015}\u{2212}"), "---");
        assert_eq!(normalize_unicode_line("\u{2018}\u{2019}"), "''");
        assert_eq!(normalize_unicode_line("\u{201C}\u{201D}"), "\"\"");
        assert_eq!(normalize_unicode_line("a\u{00A0}b"), "a b");
    }
}
