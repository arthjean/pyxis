//! Rendu markdown → lignes ratatui stylées (réponses de l'assistant). Parse via
//! pulldown-cmark (CommonMark + strikethrough/tables) et mappe les events vers
//! des `Span` en réutilisant la palette du `Theme` : headers et code = accent
//! (teal), gras/italique via modifiers, listes en puces, code-blocks indentés.
//!
//! Pensé pour le streaming : sur un markdown incomplet (tag non fermé en cours de
//! stream), pulldown ferme implicitement → le rendu se stabilise à la complétion.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render::Theme;

/// Convertit un bloc markdown en lignes prêtes à rendre (SANS gouttière : le
/// transcript ajoute son préfixe). Le texte est supposé déjà nettoyé.
pub fn render_markdown(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut r = Renderer::new(theme);
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    for ev in Parser::new_ext(text, opts) {
        r.event(ev);
    }
    r.finish()
}

struct Renderer<'t> {
    theme: &'t Theme,
    lines: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    bold: u32,
    italic: u32,
    strike: u32,
    heading: bool,
    code_block: bool,
    /// Pile de listes : `None` = puces, `Some(n)` = prochain numéro ordonné.
    list_stack: Vec<Option<u64>>,
}

impl<'t> Renderer<'t> {
    fn new(theme: &'t Theme) -> Self {
        Self {
            theme,
            lines: Vec::new(),
            cur: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            heading: false,
            code_block: false,
            list_stack: Vec::new(),
        }
    }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => {
                let st = self.theme.accent();
                self.cur.push(Span::styled(t.into_string(), st));
            }
            Event::SoftBreak => {
                let st = self.text_style();
                self.cur.push(Span::styled(" ", st));
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
            Tag::CodeBlock(_) => {
                self.flush();
                self.code_block = true;
            }
            Tag::List(start) => self.list_stack.push(start),
            Tag::Item => {
                self.flush();
                let depth = self.list_stack.len().saturating_sub(1);
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
            TagEnd::CodeBlock => {
                self.flush();
                self.code_block = false;
                self.blank();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => self.flush(),
            _ => {}
        }
    }

    /// Texte feuille : en code-block chaque ligne est indentée+dim ; sinon un
    /// span au style courant (les `\n` éventuels coupent la ligne).
    fn text(&mut self, t: &str) {
        if self.code_block {
            for (i, raw) in t.split('\n').enumerate() {
                if i > 0 {
                    self.flush();
                }
                if self.cur.is_empty() {
                    self.cur.push(Span::raw("  "));
                }
                self.cur
                    .push(Span::styled(raw.to_string(), self.theme.dim()));
            }
        } else {
            let st = self.text_style();
            for (i, raw) in t.split('\n').enumerate() {
                if i > 0 {
                    self.flush();
                }
                self.cur.push(Span::styled(raw.to_string(), st));
            }
        }
    }

    fn text_style(&self) -> Style {
        let mut st = if self.heading {
            self.theme.accent()
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

    fn flush(&mut self) {
        if !self.cur.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.cur)));
        }
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
