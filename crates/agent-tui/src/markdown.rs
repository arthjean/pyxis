//! Rendu markdown → lignes ratatui stylées (réponses de l'assistant). Parse via
//! pulldown-cmark (CommonMark + strikethrough/tables) et mappe les events vers des
//! `Span` en réutilisant la palette du `Theme` : headers et code inline = accent
//! (bleu ciel), gras/italique via modifiers, listes en puces. Les code-blocks sont
//! colorés syntaxiquement (US-042, via `highlight`), les tables alignées et les
//! blockquotes préfixées d'une barre `▎` atténuée (US-043).
//!
//! Pensé pour le streaming : sur un markdown incomplet (tag non fermé en cours de
//! stream), pulldown ferme implicitement → le rendu se stabilise à la complétion.
//! BEST-EFFORT et sans panic sur les structures malformées (table coupée au milieu).

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::measure;
use crate::theme::Theme;

/// Convertit un bloc markdown en lignes prêtes à rendre (SANS gouttière : le
/// transcript ajoute son préfixe). `width` = largeur de CONTENU disponible (après la
/// gouttière), utilisée pour dimensionner les tables. Le texte est supposé nettoyé.
pub fn render_markdown(text: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    render_markdown_with_highlight(text, theme, width, true)
}

pub(crate) fn render_markdown_with_highlight(
    text: &str,
    theme: &Theme,
    width: usize,
    highlight_code: bool,
) -> Vec<Line<'static>> {
    let mut r = Renderer::new(theme, width, highlight_code);
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    for ev in Parser::new_ext(text, opts) {
        r.event(ev);
    }
    r.finish()
}

/// Table en cours d'accumulation (les events arrivent cellule par cellule : on
/// bufferise tout avant de calculer les largeurs de colonnes à la fermeture).
#[derive(Default)]
struct TableState {
    aligns: Vec<Alignment>,
    header: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    in_cell: bool,
    cur_row: Vec<Vec<Span<'static>>>,
    cur_cell: Vec<Span<'static>>,
}

struct Renderer<'t> {
    theme: &'t Theme,
    width: usize,
    lines: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    bold: u32,
    italic: u32,
    strike: u32,
    heading: bool,
    /// Profondeur de blockquote (préfixe `▎` par niveau).
    blockquote: u32,
    /// Code-block en cours : on bufferise le contenu pour une coloration stateful.
    in_code: bool,
    code_lang: String,
    code_buf: String,
    highlight_code: bool,
    /// Pile de listes : `None` = puces, `Some(n)` = prochain numéro ordonné.
    list_stack: Vec<Option<u64>>,
    table: Option<TableState>,
}

impl<'t> Renderer<'t> {
    fn new(theme: &'t Theme, width: usize, highlight_code: bool) -> Self {
        Self {
            theme,
            width,
            lines: Vec::new(),
            cur: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            heading: false,
            blockquote: 0,
            in_code: false,
            code_lang: String::new(),
            code_buf: String::new(),
            highlight_code,
            list_stack: Vec::new(),
            table: None,
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => {
                let st = self.theme.accent();
                self.emit(Span::styled(t.into_string(), st));
            }
            Event::SoftBreak => {
                let st = self.text_style();
                self.emit(Span::styled(" ", st));
            }
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.flush();
                self.lines
                    .push(Line::from(Span::styled("─".repeat(24), self.theme.faint())));
                self.blank();
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { .. } => {
                self.flush();
                self.blank();
                self.heading = true;
            }
            Tag::Strong => self.bold += 1,
            Tag::Emphasis => self.italic += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::BlockQuote(_) => {
                self.flush();
                self.blockquote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.in_code = true;
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code_buf.clear();
            }
            Tag::List(start) => self.list_stack.push(start),
            Tag::Item => {
                self.flush();
                let depth = self.list_stack.len().saturating_sub(1).min(MAX_NEST);
                let indent = "  ".repeat(depth);
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{indent}{n}. ");
                        *n += 1;
                        m
                    }
                    _ => format!("{indent}• "),
                };
                self.cur.push(Span::styled(marker, self.theme.dim()));
            }
            Tag::Table(aligns) => {
                self.flush();
                self.blank();
                self.table = Some(TableState {
                    aligns,
                    ..Default::default()
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.cur_row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.in_cell = true;
                    t.cur_cell.clear();
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                if self.list_stack.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Heading(_) => {
                self.flush();
                self.heading = false;
                self.blank();
            }
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.blockquote = self.blockquote.saturating_sub(1);
                if self.blockquote == 0 {
                    self.blank();
                }
            }
            TagEnd::CodeBlock => self.end_code_block(),
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => self.flush(),
            TagEnd::Table => self.end_table(),
            TagEnd::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.header = std::mem::take(&mut t.cur_row);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    let row = std::mem::take(&mut t.cur_row);
                    if t.rows.len() < MAX_ROWS {
                        t.rows.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.in_cell = false;
                    let cell = std::mem::take(&mut t.cur_cell);
                    if t.cur_row.len() < MAX_COLS {
                        t.cur_row.push(cell);
                    }
                }
            }
            _ => {}
        }
    }

    /// Texte feuille : bufferisé si code-block ; routé en cellule si table ; sinon un
    /// span au style courant (les `\n` coupent la ligne).
    fn text(&mut self, t: &str) {
        if self.in_code {
            self.code_buf.push_str(t);
            return;
        }
        if self.table.is_some() {
            let st = self.text_style();
            self.emit(Span::styled(t.to_string(), st));
            return;
        }
        let st = self.text_style();
        for (i, raw) in t.split('\n').enumerate() {
            if i > 0 {
                self.flush();
            }
            self.cur.push(Span::styled(raw.to_string(), st));
        }
    }

    /// Route un span vers la cellule de table active, sinon vers la ligne courante.
    /// Dans une table mais hors cellule (malformé), le span est ignoré (best-effort).
    fn emit(&mut self, span: Span<'static>) {
        if let Some(t) = self.table.as_mut() {
            if t.in_cell && t.cur_row.len() < MAX_COLS && cell_width(&t.cur_cell) < MAX_CELL_WIDTH {
                let remaining = MAX_CELL_WIDTH.saturating_sub(cell_width(&t.cur_cell));
                let text = measure::truncate(span.content.as_ref(), remaining);
                t.cur_cell.push(Span::styled(text, span.style));
            }
            return;
        }
        self.cur.push(span);
    }

    fn text_style(&self) -> Style {
        let mut st = if self.heading {
            self.theme.accent()
        } else if self.blockquote > 0 {
            self.theme.dim()
        } else {
            self.theme.fg()
        };
        if self.heading || self.bold > 0 {
            st = st.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            st = st.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            st = st.add_modifier(Modifier::CROSSED_OUT);
        }
        st
    }

    /// Émet le code-block accumulé : coloré syntaxiquement si possible (US-042),
    /// sinon en dim indenté (repli neutre). Chaque ligne est indentée de 2 colonnes.
    fn end_code_block(&mut self) {
        self.in_code = false;
        let code = std::mem::take(&mut self.code_buf);
        let lang = std::mem::take(&mut self.code_lang);
        if should_unwrap_markdown_table(&lang, &code) {
            self.lines.extend(render_markdown_with_highlight(
                &code, self.theme, self.width, false,
            ));
            self.blank();
            return;
        }
        match self
            .highlight_code
            .then(|| crate::highlight::code_block(&code, &lang, self.theme))
            .flatten()
        {
            Some(rows) => {
                for spans in rows {
                    let mut line = vec![Span::raw("  ")];
                    line.extend(spans);
                    self.lines.push(Line::from(line));
                }
            }
            None => {
                for raw in code.lines() {
                    self.lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(raw.to_string(), self.theme.dim()),
                    ]));
                }
            }
        }
        self.blank();
    }

    /// Émet la table accumulée (grille alignée, ou repli clé/valeur si trop large).
    fn end_table(&mut self) {
        if let Some(t) = self.table.take() {
            self.lines.extend(render_table(&t, self.theme, self.width));
            self.blank();
        }
    }

    fn flush(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let mut spans = std::mem::take(&mut self.cur);
        if self.blockquote > 0 {
            let depth = (self.blockquote as usize).min(MAX_NEST);
            let bar = Span::styled("▎ ".repeat(depth), self.theme.faint());
            spans.insert(0, bar);
        }
        self.lines.push(Line::from(spans));
    }

    /// Ligne vide, dédupliquée (pas deux vides consécutives).
    fn blank(&mut self) {
        if self.lines.last().map(|l| l.spans.is_empty()) != Some(true) {
            self.lines.push(Line::default());
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        while self.lines.first().map(|l| l.spans.is_empty()) == Some(true) {
            self.lines.remove(0);
        }
        while self.lines.last().map(|l| l.spans.is_empty()) == Some(true) {
            self.lines.pop();
        }
        self.lines
    }
}

/// Largeur d'affichage d'une cellule (somme des largeurs de ses spans, en chars:
/// cohérent avec le reste du wrap du transcript).
fn cell_width(cell: &[Span<'static>]) -> usize {
    cell.iter()
        .map(|s| measure::width(s.content.as_ref()))
        .sum()
}

/// Met en gras les spans d'une cellule (en-tête de table mis en évidence).
fn bolden(cell: &[Span<'static>]) -> Vec<Span<'static>> {
    cell.iter()
        .map(|s| Span::styled(s.content.to_string(), s.style.add_modifier(Modifier::BOLD)))
        .collect()
}

/// Largeur du séparateur de colonnes ` │ ` (3 chars).
const SEP_W: usize = 3;
/// Profondeur d'imbrication max prise en compte pour l'INDENTATION (liste /
/// blockquote) : borne le coût d'allocation `repeat()` sur un markdown adverse
/// profondément imbriqué (sinon O(n²) par ligne). Au-delà, l'indent sature.
const MAX_NEST: usize = 24;
/// Colonnes max d'une table rendue : borne l'allocation (`col_w`, `sep_line`) sur
/// une table adverse à des milliers de colonnes. Au-delà → colonnes excédentaires
/// ignorées (best-effort, comme une cellule manquante).
const MAX_COLS: usize = 64;
const MAX_ROWS: usize = 256;
const MAX_CELL_WIDTH: usize = 4096;

/// Rend une table : grille alignée si elle tient dans `width`, sinon repli en paires
/// `clé: valeur` (chaque cellule sur sa ligne, préfixée de son en-tête). Best-effort
/// sur les tables malformées (lignes ragged : cellules manquantes tolérées).
/// Note : une table imbriquée dans un blockquote n'hérite pas de la barre `▎`
/// (cas rare, cosmétique): les lignes de table contournent `flush`.
fn render_table(t: &TableState, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let ncols = t
        .header
        .len()
        .max(t.rows.iter().map(Vec::len).max().unwrap_or(0))
        .max(t.aligns.len())
        .min(MAX_COLS);
    if ncols == 0 {
        return Vec::new();
    }

    let mut col_w = vec![0usize; ncols];
    for (i, cell) in t.header.iter().enumerate() {
        if let Some(w) = col_w.get_mut(i) {
            *w = (*w).max(cell_width(cell));
        }
    }
    for row in &t.rows {
        for (i, cell) in row.iter().enumerate() {
            if let Some(w) = col_w.get_mut(i) {
                *w = (*w).max(cell_width(cell));
            }
        }
    }

    // Saturant : pas de panic d'overflow (mode debug) même sur des cellules énormes.
    let grid_w = col_w
        .iter()
        .copied()
        .fold(0usize, usize::saturating_add)
        .saturating_add(SEP_W.saturating_mul(ncols - 1));
    let mut out = Vec::new();
    if grid_w <= width.max(1) {
        if !t.header.is_empty() {
            out.push(grid_row(&t.header, &col_w, &t.aligns, theme, true));
            out.push(sep_line(&col_w, theme));
        }
        for row in &t.rows {
            out.push(grid_row(row, &col_w, &t.aligns, theme, false));
        }
    } else {
        // Repli clé/valeur : lisible sur terminal étroit sans déborder ni corrompre.
        for (ri, row) in t.rows.iter().enumerate() {
            if ri > 0 {
                out.push(Line::default());
            }
            for (i, cell) in row.iter().enumerate() {
                let mut spans = Vec::new();
                if let Some(h) = t.header.get(i) {
                    spans.extend(bolden(h));
                    spans.push(Span::styled(": ", theme.faint()));
                }
                spans.extend(cell.iter().cloned());
                out.push(Line::from(spans));
            }
        }
    }
    out
}

/// Une ligne de grille : cellules paddées à la largeur de colonne selon l'alignement,
/// séparées par ` │ ` (faint). En-tête → cellules en gras.
fn grid_row(
    cells: &[Vec<Span<'static>>],
    col_w: &[usize],
    aligns: &[Alignment],
    theme: &Theme,
    header: bool,
) -> Line<'static> {
    let empty: Vec<Span<'static>> = Vec::new();
    let mut spans = Vec::new();
    for (i, cw) in col_w.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │ ", theme.faint()));
        }
        let cell = cells.get(i).unwrap_or(&empty);
        let pad = cw.saturating_sub(cell_width(cell));
        let (lp, rp) = match aligns.get(i).copied().unwrap_or(Alignment::None) {
            Alignment::Right => (pad, 0),
            Alignment::Center => (pad / 2, pad - pad / 2),
            _ => (0, pad),
        };
        if lp > 0 {
            spans.push(Span::raw(" ".repeat(lp)));
        }
        if header {
            spans.extend(bolden(cell));
        } else {
            spans.extend(cell.iter().cloned());
        }
        if rp > 0 {
            spans.push(Span::raw(" ".repeat(rp)));
        }
    }
    Line::from(spans)
}

/// Filet de séparation sous l'en-tête (`───┼───`, faint).
fn sep_line(col_w: &[usize], theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, w) in col_w.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("─┼─", theme.faint()));
        }
        spans.push(Span::styled("─".repeat(*w), theme.faint()));
    }
    Line::from(spans)
}

fn should_unwrap_markdown_table(lang: &str, code: &str) -> bool {
    if !matches!(lang.trim().to_ascii_lowercase().as_str(), "markdown" | "md") {
        return false;
    }
    let lines: Vec<_> = code
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() < 2 {
        return false;
    }
    let has_separator = lines.iter().any(|line| {
        line.chars().all(|ch| matches!(ch, '|' | '-' | ':' | ' '))
            && line.contains('-')
            && line.matches('|').count() >= 2
    });
    has_separator && lines.iter().all(|line| line.matches('|').count() >= 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Aplati chaque ligne en sa chaîne de caractères (styles ignorés).
    fn flat(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn table_renders_aligned_with_header() {
        let theme = Theme::new(true);
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        let text = flat(&render_markdown(md, &theme, 80));
        assert!(
            text.iter().any(|l| l.contains('A') && l.contains('B')),
            "en-tête"
        );
        assert!(
            text.iter().any(|l| l.contains('1') && l.contains('2')),
            "données"
        );
        assert!(
            text.iter().any(|l| l.contains('│')),
            "séparateur de colonnes"
        );
        assert!(text.iter().any(|l| l.contains('─')), "filet d'en-tête");
    }

    #[test]
    fn wide_table_degrades_to_key_value() {
        let theme = Theme::new(true);
        let md = "| Header One | Header Two |\n|---|---|\n| valeurlongue | autrelongue |\n";
        // Largeur trop étroite pour la grille → bascule clé/valeur.
        let text = flat(&render_markdown(md, &theme, 14));
        assert!(
            text.iter()
                .any(|l| l.contains("Header One") && l.contains(':')),
            "expected key/value fallback, got: {text:?}"
        );
    }

    #[test]
    fn blockquote_gets_bar_prefix() {
        let theme = Theme::new(true);
        let text = flat(&render_markdown("> citation\n", &theme, 80));
        assert!(
            text.iter()
                .any(|l| l.contains('▎') && l.contains("citation"))
        );
    }

    #[test]
    fn code_block_without_truecolor_is_neutral_text() {
        let theme = Theme::new(false); // pas de coloration → repli dim
        let text = flat(&render_markdown("```rust\nlet x = 1;\n```\n", &theme, 80));
        assert!(text.iter().any(|l| l.contains("let x = 1;")));
    }

    #[test]
    fn code_block_unknown_language_is_neutral_text() {
        let theme = Theme::new(true);
        let text = flat(&render_markdown(
            "```langage-bidon\nfoo bar\n```\n",
            &theme,
            80,
        ));
        assert!(text.iter().any(|l| l.contains("foo bar")));
    }

    #[test]
    fn markdown_table_fence_unwraps_only_for_plain_tables() {
        let theme = Theme::new(true);
        let table = flat(&render_markdown(
            "```markdown\n| A | B |\n|---|---|\n| 1 | 2 |\n```\n",
            &theme,
            80,
        ));
        assert!(table.iter().any(|line| line.contains('│')));

        let mixed = flat(&render_markdown(
            "```markdown\n| A | B |\n|---|---|\ntext\n```\n",
            &theme,
            80,
        ));
        assert!(
            !mixed.iter().any(|line| line.contains('│')),
            "mixed markdown fences stay as code"
        );
    }

    #[test]
    fn malformed_table_does_not_panic() {
        let theme = Theme::new(true);
        // Table coupée en plein milieu (stream interrompu).
        let _ = render_markdown("| A | B |\n|---|---|\n| 1 ", &theme, 80);
        // Largeur dégénérée.
        let _ = render_markdown("| A | B |\n|---|---|\n| 1 | 2 |\n", &theme, 0);
    }

    // Sécurité (DoS contenu adverse) : imbrication profonde et table à très nombreuses
    // colonnes sont BORNÉES (pas de freeze O(n²) ni d'allocation géante), sans panic.
    #[test]
    fn adversarial_nesting_and_wide_table_are_bounded() {
        let theme = Theme::new(true);
        // 500 niveaux de blockquote → barres `▎` bornées à MAX_NEST.
        let deep = format!("{} x\n", ">".repeat(500));
        let lines = render_markdown(&deep, &theme, 80);
        let max_bars = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.matches('▎').count())
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0);
        assert!(max_bars <= MAX_NEST, "barres non bornées: {max_bars}");
        // Table à 300 colonnes → rendue bornée à MAX_COLS, sans panic.
        let cells = (0..300).map(|_| "x").collect::<Vec<_>>().join(" | ");
        let seps = (0..300).map(|_| "---").collect::<Vec<_>>().join("|");
        let md = format!("| {cells} |\n|{seps}|\n| {cells} |\n");
        let out = render_markdown(&md, &theme, 200);
        assert!(!out.is_empty(), "table large rendue sans panic");
    }

    #[test]
    fn plain_paragraph_unchanged() {
        let theme = Theme::new(true);
        let text = flat(&render_markdown("hello **world**", &theme, 80));
        assert_eq!(text, vec!["hello world".to_string()]);
    }
}
