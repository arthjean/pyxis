//! Disposition du composer multi-ligne (US-010, `tasks/prd-harness-parity.md`).
//!
//! Le modèle de saisie reste un `String` plat avec un curseur en offset byte
//! (`AppState::input`) : toute la logique de menu et les tests existants lisent
//! l'input comme un `&str`. Le multi-ligne est donc DÉRIVÉ ici, à la largeur de
//! rendu, plutôt que stocké sous forme de `Vec<String>`.
//!
//! Invariant central : les lignes visuelles PARTITIONNENT l'input. Concaténées
//! dans l'ordre, séparateurs `\n` réinsérés, elles redonnent l'input octet pour
//! octet. C'est cet invariant qui garantit qu'aucun caractère n'est perdu au
//! repli (AC1) et que la position écran du curseur correspond exactement à sa
//! position logique (AC4).

use std::iter::Peekable;
use std::str::Chars;

use unicode_segmentation::UnicodeSegmentation;

use crate::measure;

/// Un segment contigu de l'input occupant une ligne à l'écran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VisualRow {
    /// Offset byte de début dans l'input (inclus).
    pub start: usize,
    /// Offset byte de fin dans l'input (exclu).
    pub end: usize,
    /// Premier segment d'une ligne logique (pas une continuation de repli).
    pub first_of_line: bool,
}

/// Résultat du calcul de disposition pour une largeur donnée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
    pub rows: Vec<VisualRow>,
    /// Index de la ligne visuelle portant le curseur.
    pub cursor_row: usize,
    /// Colonne du curseur dans la zone texte (gouttière exclue), en cellules.
    pub cursor_col: usize,
}

/// Replie `input` sur `width` colonnes et localise le curseur.
///
/// `width` est la largeur de la zone TEXTE (gouttière déjà retranchée) ; une
/// largeur nulle est ramenée à 1 pour que l'algorithme progresse toujours.
pub(crate) fn layout(input: &str, cursor: usize, width: usize) -> Layout {
    let width = width.max(1);
    let mut rows: Vec<VisualRow> = Vec::new();

    let mut line_start = 0usize;
    for line in input.split('\n') {
        let line_end = line_start + line.len();
        let mut seg_start = line_start;
        loop {
            let seg_end = seg_start + wrap_point(&input[seg_start..line_end], width);
            rows.push(VisualRow {
                start: seg_start,
                end: seg_end,
                first_of_line: seg_start == line_start,
            });
            if seg_end >= line_end {
                break;
            }
            seg_start = seg_end;
        }
        // +1 : le `\n` consommé par `split` n'appartient à aucun segment.
        line_start = line_end + 1;
    }

    let cursor = cursor.min(input.len());
    let (mut cursor_row, mut cursor_col) = (rows.len().saturating_sub(1), 0);
    for (idx, row) in rows.iter().enumerate() {
        if cursor < row.start || cursor > row.end {
            continue;
        }
        cursor_row = idx;
        cursor_col = measure::width(&input[row.start..cursor]);
        // Curseur en fin de segment REPLIÉ : il s'affiche au début de la
        // continuation, là où le prochain caractère saisi apparaîtra.
        if cursor == row.end && rows.get(idx + 1).is_some_and(|next| !next.first_of_line) {
            cursor_row = idx + 1;
            cursor_col = 0;
        }
        break;
    }
    // Un segment exactement plein place le curseur une colonne au-delà du bord
    // quand il n'y a pas de continuation (fin de saisie) : on le ramène dans la
    // zone plutôt que de dessiner hors cadre.
    cursor_col = cursor_col.min(width.saturating_sub(1));

    Layout {
        rows,
        cursor_row,
        cursor_col,
    }
}

/// Premier offset byte où couper `s` pour tenir dans `width` colonnes.
///
/// Retourne `s.len()` si tout tient. Sinon coupe après le dernier espace
/// rencontré (l'espace reste sur la ligne courante, donc la partition est
/// préservée), ou en dur si aucun espace n'offre de point de coupure. Progresse
/// toujours d'au moins un graphème : une largeur de 1 face à un caractère large
/// ne peut pas boucler.
fn wrap_point(s: &str, width: usize) -> usize {
    let mut used = 0usize;
    let mut last_space_end: Option<usize> = None;
    for (i, g) in s.grapheme_indices(true) {
        let w = measure::width(g);
        if used + w > width {
            let hard = if i == 0 { g.len() } else { i };
            return match last_space_end {
                Some(sp) if sp > 0 && sp < hard => sp,
                _ => hard,
            };
        }
        used += w;
        if g == " " {
            last_space_end = Some(i + g.len());
        }
    }
    s.len()
}

/// Première ligne visible pour que `cursor_row` reste dans la fenêtre.
pub(crate) fn scroll_offset(cursor_row: usize, total_rows: usize, visible: usize) -> usize {
    let visible = visible.max(1);
    if total_rows <= visible {
        return 0;
    }
    let max_offset = total_rows - visible;
    cursor_row
        .saturating_sub(visible - 1)
        .min(max_offset)
        .min(cursor_row)
}

/// Largeur d'une tabulation collée, convertie en espaces.
const TAB_WIDTH: usize = 4;

/// Neutralise un contenu collé avant qu'il n'entre dans le composer (US-011).
///
/// Les séquences d'échappement ANSI sont retirées ENTIÈREMENT (introducteur et
/// paramètres) : ne supprimer que l'octet `ESC` laisserait `[31m` en texte
/// visible. Les autres caractères de contrôle sont supprimés, sauf `\n`. `\r\n`
/// et `\r` isolé deviennent un simple `\n`.
///
/// La tabulation est convertie en espaces : `unicode-width` lui donne une
/// largeur nulle alors que le terminal l'étend jusqu'au taquet suivant. Laissée
/// telle quelle, elle décalerait le cadre entier, exactement comme une séquence
/// ANSI, sans qu'aucune mesure de largeur ne le voie venir.
///
/// La neutralisation s'applique au contenu STOCKÉ, pas seulement à l'affichage :
/// le modèle n'a aucun usage d'une séquence de contrôle terminal, et le contenu
/// envoyé doit être celui qui a été relu à l'écran.
pub(crate) fn sanitize_paste(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push(c),
            '\t' => out.push_str(&" ".repeat(TAB_WIDTH)),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Consomme le corps d'une séquence d'échappement, `ESC` déjà lu.
fn skip_escape(chars: &mut Peekable<Chars<'_>>) {
    match chars.next() {
        // CSI : paramètres puis un octet final dans 0x40..=0x7E.
        Some('[') => {
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
        // OSC / DCS / SOS / PM / APC : terminés par BEL ou par ST (`ESC \`).
        Some(']' | 'P' | 'X' | '^' | '_') => {
            while let Some(c) = chars.next() {
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' {
                    chars.next_if(|next| *next == '\\');
                    break;
                }
            }
        }
        // Échappement à deux caractères (`ESC c`, `ESC (B`…) : déjà consommé.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'invariant de partition : les segments recollés redonnent l'input.
    fn recompose(input: &str, layout: &Layout) -> String {
        let mut out = String::new();
        for (idx, row) in layout.rows.iter().enumerate() {
            if idx > 0 && row.first_of_line {
                out.push('\n');
            }
            out.push_str(&input[row.start..row.end]);
        }
        out
    }

    #[test]
    fn wrapping_loses_no_character() {
        for input in [
            "",
            "court",
            "un texte nettement plus long que la largeur donnee",
            "ligne1\nligne2\n\nligne4",
            "motbeaucouptroplongpourtenirsurunelignedonnee",
            "  espaces   multiples   preserves  ",
            "héllo wörld ✅ 漢字テスト",
        ] {
            for width in [1usize, 2, 3, 7, 20, 80] {
                let l = layout(input, 0, width);
                assert_eq!(recompose(input, &l), input, "input={input:?} width={width}");
            }
        }
    }

    #[test]
    fn no_visual_row_exceeds_width() {
        let input = "un texte assez long pour etre replie plusieurs fois de suite";
        for width in [1usize, 4, 9, 17] {
            let l = layout(input, 0, width);
            for row in &l.rows {
                assert!(
                    measure::width(&input[row.start..row.end]) <= width,
                    "segment {:?} depasse {width}",
                    &input[row.start..row.end]
                );
            }
        }
    }

    #[test]
    fn explicit_newlines_produce_one_row_each() {
        let input = "a\n\nb";
        let l = layout(input, 0, 80);
        assert_eq!(l.rows.len(), 3);
        assert!(l.rows.iter().all(|r| r.first_of_line));
    }

    #[test]
    fn cursor_maps_to_logical_position_on_wrapped_line() {
        let input = "aaaa bbbb cccc";
        // width 10 → « aaaa bbbb » puis « cccc ».
        let l = layout(input, 0, 10);
        assert_eq!(l.rows.len(), 2);
        let at_c = layout(input, 10, 10);
        assert_eq!((at_c.cursor_row, at_c.cursor_col), (1, 0));
        let end = layout(input, input.len(), 10);
        assert_eq!((end.cursor_row, end.cursor_col), (1, 4));
    }

    #[test]
    fn cursor_at_end_of_logical_line_stays_on_that_line() {
        let input = "ab\ncd";
        let l = layout(input, 2, 80);
        assert_eq!((l.cursor_row, l.cursor_col), (0, 2));
    }

    #[test]
    fn cursor_column_counts_terminal_cells_not_bytes() {
        let input = "漢字x";
        let l = layout(input, "漢字".len(), 80);
        assert_eq!(l.cursor_col, 4);
    }

    #[test]
    fn wide_grapheme_never_stalls_on_narrow_width() {
        let input = "漢漢漢";
        let l = layout(input, 0, 1);
        assert_eq!(l.rows.len(), 3);
        assert_eq!(recompose(input, &l), input);
    }

    #[test]
    fn scroll_keeps_cursor_row_visible() {
        assert_eq!(scroll_offset(0, 20, 5), 0);
        assert_eq!(scroll_offset(4, 20, 5), 0);
        assert_eq!(scroll_offset(5, 20, 5), 1);
        assert_eq!(scroll_offset(19, 20, 5), 15);
        assert_eq!(scroll_offset(3, 3, 5), 0);
    }

    #[test]
    fn sanitize_strips_full_ansi_sequences() {
        assert_eq!(sanitize_paste("\u{1b}[31mrouge\u{1b}[0m"), "rouge");
        assert_eq!(sanitize_paste("\u{1b}]0;titre\u{7}ok"), "ok");
        assert_eq!(sanitize_paste("\u{1b}]8;;http://x\u{1b}\\lien"), "lien");
        assert_eq!(sanitize_paste("a\u{1b}[2Jb"), "ab");
        assert_eq!(sanitize_paste("\u{1b}c reset"), " reset");
    }

    #[test]
    fn sanitize_keeps_newlines_expands_tabs_drops_other_controls() {
        assert_eq!(sanitize_paste("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(sanitize_paste("a\tb"), "a    b");
        assert_eq!(sanitize_paste("a\u{0}b\u{7}c\u{9b}d"), "abcd");
        assert_eq!(sanitize_paste("héllo 漢字 ✅"), "héllo 漢字 ✅");
    }

    /// Aucun caractère survivant à la neutralisation ne peut déplacer le curseur
    /// du terminal : ni `ESC`, ni un autre contrôle, ni une tabulation.
    #[test]
    fn sanitized_paste_contains_no_cursor_moving_character() {
        let hostile = "\u{1b}[2J\u{1b}[1;1Heffacé\ttab\u{8}\u{c}\u{1b}]0;titre\u{7}fin\r\nsuite";
        let clean = sanitize_paste(hostile);
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.chars().any(|c| c.is_control() && c != '\n'));
        assert_eq!(clean, "effacé    tabfin\nsuite");
    }
}
