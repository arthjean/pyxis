//! Inline terminal with a viewport whose height follows the content.
//!
//! Derived from `ratatui::Terminal` (MIT, see the header below). Ratatui freezes
//! the height of a `Viewport::Inline` at construction time and exposes no way to
//! change it, so the parity renderer cannot shrink the drawn area down to
//! "active cell + bottom pane" and leave the rest of the screen to the native
//! scrollback. This derivation makes `viewport_area` writable, which is the one
//! capability the upstream type withholds.
//!
//! Everything else stays deliberately close to upstream: double buffering,
//! `Buffer::diff` against the previous frame, cursor bookkeeping.
//!
// This file is derived from `ratatui::Terminal`, which is licensed under the
// following terms:
//
// The MIT License (MIT)
// Copyright (c) 2016-2022 Florian Dehau
// Copyright (c) 2023-2025 The Ratatui Developers
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use std::io;

use ratatui::backend::{Backend, ClearType};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect, Size};
use ratatui::widgets::Widget;

/// Render target handed to the draw callback.
///
/// Mirrors the subset of `ratatui::Frame` the Pyxis renderer actually uses.
/// `ratatui::Frame` cannot be constructed outside its own crate, so a viewport
/// we own needs a frame we own.
pub struct Frame<'a> {
    cursor_position: Option<Position>,
    viewport_area: Rect,
    buffer: &'a mut Buffer,
}

impl Frame<'_> {
    /// Area available to the frame: the viewport, in screen coordinates.
    pub fn area(&self) -> Rect {
        self.viewport_area
    }

    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }

    pub fn render_widget<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.buffer);
    }

    /// Places the terminal cursor after the frame is flushed. Leaving it unset
    /// hides the cursor for that frame.
    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) {
        self.cursor_position = Some(position.into());
    }
}

/// Inline terminal owning a viewport anchored at the bottom of the screen.
///
/// The rows above `viewport_area` belong to the terminal scrollback and are
/// never redrawn: [`crate::insert_history`] writes finalized history there once,
/// and the terminal keeps it.
#[derive(Debug)]
pub struct Terminal<B: Backend> {
    backend: B,
    buffers: [Buffer; 2],
    current: usize,
    hidden_cursor: bool,
    /// Area currently owned by the renderer. Public on purpose: resizing it is
    /// the reason this type exists.
    pub viewport_area: Rect,
    pub last_known_screen_size: Size,
    last_known_cursor_pos: Position,
    /// History rows this renderer wrote that are still on screen.
    ///
    /// Rows that scrolled past the top belong to the terminal's scrollback and
    /// can no longer be rewritten; only what is still visible can be repaired
    /// after a resize, and only that may be cleared.
    visible_history_rows: u16,
}

impl<B: Backend> Drop for Terminal<B> {
    fn drop(&mut self) {
        if self.hidden_cursor
            && let Err(err) = self.show_cursor()
        {
            eprintln!("Failed to show the cursor: {err}");
        }
    }
}

impl<B: Backend> Terminal<B> {
    /// Builds a terminal anchored at the current cursor row, with an empty
    /// viewport. The first [`Terminal::draw`] grows it to the requested height.
    ///
    /// A terminal that does not answer the cursor-position request (`ESC[6n`)
    /// must not abort startup: the origin is a safe anchor, the first draw
    /// re-anchors at the bottom of the screen anyway.
    pub fn new(mut backend: B) -> io::Result<Self> {
        let screen_size = backend.size()?;
        let cursor_pos = backend
            .get_cursor_position()
            .unwrap_or(Position { x: 0, y: 0 });
        Ok(Self::with_geometry(backend, screen_size, cursor_pos))
    }

    /// Builds a terminal whose viewport covers the whole screen, without
    /// touching the backend. Used by the snapshot harness and the previews,
    /// which have no real cursor to probe.
    pub fn full_screen(backend: B, width: u16, height: u16) -> Self {
        let mut terminal =
            Self::with_geometry(backend, Size::new(width, height), Position { x: 0, y: 0 });
        terminal.set_viewport_area(Rect::new(0, 0, width, height));
        terminal
    }

    /// Builds a terminal from a known geometry, without touching the backend.
    /// Used by the snapshot harness, which has no real cursor to probe.
    pub fn with_geometry(backend: B, screen_size: Size, cursor_pos: Position) -> Self {
        Self {
            backend,
            buffers: [Buffer::empty(Rect::ZERO), Buffer::empty(Rect::ZERO)],
            current: 0,
            hidden_cursor: false,
            viewport_area: Rect::new(0, cursor_pos.y, 0, 0),
            last_known_screen_size: screen_size,
            last_known_cursor_pos: cursor_pos,
            visible_history_rows: 0,
        }
    }

    pub fn get_frame(&mut self) -> Frame<'_> {
        let viewport_area = self.viewport_area;
        Frame {
            cursor_position: None,
            viewport_area,
            buffer: &mut self.buffers[self.current],
        }
    }

    pub fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }

    /// Moves and resizes the drawn area. Both buffers follow, so the next diff
    /// compares like for like.
    pub fn set_viewport_area(&mut self, area: Rect) {
        self.buffers[self.current].resize(area);
        self.buffers[1 - self.current].resize(area);
        self.viewport_area = area;
        // Nothing above the viewport top is ours any more.
        self.visible_history_rows = self.visible_history_rows.min(area.top());
    }

    /// Records history rows written directly above the viewport.
    pub fn note_history_rows(&mut self, rows: u16) {
        self.visible_history_rows = self
            .visible_history_rows
            .saturating_add(rows)
            .min(self.viewport_area.top());
    }

    pub fn visible_history_rows(&self) -> u16 {
        self.visible_history_rows
    }

    /// Forgets what the previous frame painted, so the next draw re-emits every
    /// cell. Required after anything wrote to the screen behind our back
    /// (history insertion, scroll, external clear).
    pub fn invalidate_viewport(&mut self) {
        self.buffers[1 - self.current].reset();
    }

    pub fn record_screen_size(&mut self, screen_size: Size) {
        self.last_known_screen_size = screen_size;
    }

    /// Keeps the viewport against the bottom of the screen, tall enough for
    /// `min_height` rows of content.
    ///
    /// The bottom edge never moves: that is where the composer sits, and an
    /// input line that drifts up the screen as the transcript grows is the thing
    /// this anchoring exists to prevent. The viewport therefore spans from
    /// wherever history stopped down to the last row, and only its top moves.
    ///
    /// Growing past that top steals rows from the history above, so those rows
    /// are scrolled up into the scrollback rather than overwritten. Shrinking
    /// happens elsewhere: [`crate::insert_history`] takes rows from the top as it
    /// writes history into them.
    pub fn anchor_viewport(&mut self, min_height: u16) -> io::Result<()> {
        let size = self.size()?;
        let previous_screen_height = self.last_known_screen_size.height;
        self.record_screen_size(size);
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }

        // A screen that lost rows scrolled its content up by the difference, the
        // viewport included: its recorded position is stale. Re-anchoring on it
        // would clear from below the rows that moved and leave the previous
        // frame's top on screen, as a ghost of the card above the new one.
        if size.height < previous_screen_height {
            let shift = previous_screen_height - size.height;
            let moved = Rect {
                y: self.viewport_area.y.saturating_sub(shift),
                ..self.viewport_area
            };
            self.set_viewport_area(moved);
            // The terminal moved those cells behind our back, so the previous
            // buffer no longer describes the screen: every cell must be re-sent.
            self.invalidate_viewport();
        }

        let previous = self.viewport_area;
        let mut area = previous;
        area.x = 0;
        area.width = size.width;
        // Height the viewport already has if it reaches the bottom, which is the
        // invariant this function restores.
        let anchored = size.height.saturating_sub(previous.top());
        area.height = min_height.max(anchored).clamp(1, size.height);
        area.y = size.height - area.height;

        if area.y < previous.y {
            let taken = previous.y - area.y;
            if previous.y > 0 {
                self.backend
                    .scroll_region_up(0..previous.y, taken.min(previous.y))?;
            }
        }

        if area != previous {
            let clear_from = previous.y.min(area.y);
            self.set_viewport_area(area);
            self.clear_after_row(clear_from)?;
        }
        Ok(())
    }

    /// Draws one frame: renders into the current buffer, flushes the diff, then
    /// places or hides the cursor.
    pub fn draw<F>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        self.buffers[self.current].reset();
        let mut frame = self.get_frame();
        render_callback(&mut frame);
        let cursor_position = frame.cursor_position;

        self.flush()?;

        match cursor_position {
            None => self.hide_cursor()?,
            Some(position) => {
                self.show_cursor()?;
                self.set_cursor_position(position)?;
            }
        }

        self.swap_buffers();
        self.backend.flush()?;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        let previous = &self.buffers[1 - self.current];
        let current = &self.buffers[self.current];
        let updates = previous.diff(current);
        if let Some((col, row, _)) = updates.last() {
            self.last_known_cursor_pos = Position { x: *col, y: *row };
        }
        self.backend.draw(updates.into_iter())
    }

    fn swap_buffers(&mut self) {
        self.current = 1 - self.current;
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()?;
        self.hidden_cursor = true;
        Ok(())
    }

    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.backend.show_cursor()?;
        self.hidden_cursor = false;
        Ok(())
    }

    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.backend.set_cursor_position(position)?;
        self.last_known_cursor_pos = position;
        Ok(())
    }

    pub fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.backend.get_cursor_position()
    }

    /// Clears everything from `row` to the bottom of the screen.
    ///
    /// Used when the viewport moves or shrinks: the rows it no longer owns still
    /// show its previous frame, and nothing else will ever repaint them.
    pub fn clear_after_row(&mut self, row: u16) -> io::Result<()> {
        self.backend
            .set_cursor_position(Position { x: 0, y: row })?;
        self.backend.clear_region(ClearType::AfterCursor)?;
        self.invalidate_viewport();
        Ok(())
    }

    /// Clears the viewport rows only; the scrollback above is left alone.
    pub fn clear_viewport(&mut self) -> io::Result<()> {
        let row = self.viewport_area.y;
        self.clear_after_row(row)
    }

    /// Clears the whole screen, scrollback excluded.
    pub fn clear_screen(&mut self) -> io::Result<()> {
        self.clear_after_row(0)
    }

    /// Clears the history rows this renderer still owns, plus the viewport, and
    /// re-anchors an empty viewport where they started. Returns how many rows
    /// were freed.
    ///
    /// Used by resize reflow, which is about to rewrite those rows at the new
    /// width: leaving them would show the same messages twice, once wrapped for
    /// a width the terminal no longer has. Rows that already scrolled into the
    /// scrollback are left alone: they are the terminal's, and so is whatever
    /// the user had on screen before this session started.
    pub fn clear_owned_history(&mut self) -> io::Result<u16> {
        let rows = self.visible_history_rows;
        let top = self.viewport_area.top().saturating_sub(rows);
        self.backend
            .set_cursor_position(Position { x: 0, y: top })?;
        self.backend.clear_region(ClearType::AfterCursor)?;
        let width = self.viewport_area.width;
        self.set_viewport_area(Rect::new(0, top, width, 0));
        self.visible_history_rows = 0;
        self.invalidate_viewport();
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;

    /// Buffer -> text, so an assertion can name what the screen actually shows.
    fn dump(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn terminal(width: u16, height: u16) -> Terminal<TestBackend> {
        Terminal::with_geometry(
            TestBackend::new(width, height),
            Size::new(width, height),
            Position { x: 0, y: 0 },
        )
    }

    #[test]
    fn the_viewport_can_grow_and_shrink_between_frames() {
        let mut terminal = terminal(10, 6);

        terminal.set_viewport_area(Rect::new(0, 4, 10, 2));
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("ab\ncd"), frame.area()))
            .expect("premier rendu");
        assert_eq!(terminal.viewport_area.height, 2);

        terminal.set_viewport_area(Rect::new(0, 3, 10, 3));
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("ef\ngh\nij"), frame.area()))
            .expect("second rendu");

        assert_eq!(terminal.viewport_area, Rect::new(0, 3, 10, 3));
        let screen = dump(terminal.backend().buffer());
        assert!(screen.contains("ij"), "dernière ligne rendue: {screen}");
    }

    /// The buffers must follow the viewport, otherwise the diff compares cells
    /// that no longer live at the same screen row and the frame paints garbage.
    #[test]
    fn moving_the_viewport_resizes_both_buffers() {
        let mut terminal = terminal(8, 5);
        terminal.set_viewport_area(Rect::new(0, 1, 8, 3));
        assert_eq!(terminal.current_buffer_mut().area, Rect::new(0, 1, 8, 3));
        terminal.swap_buffers();
        assert_eq!(terminal.current_buffer_mut().area, Rect::new(0, 1, 8, 3));
    }
}
