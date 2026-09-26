//! Browse buckets and folders, find stored email, save a folder as the inbox.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use super::{Ctx, Transition, View};

pub struct BrowserScreen {}

impl BrowserScreen {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for BrowserScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl View for BrowserScreen {
    fn title(&self) -> String {
        "Browse S3".into()
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        frame.render_widget(Paragraph::new("Browse S3: not built yet"), area);
    }

    fn on_key(&mut self, key: KeyEvent, _ctx: &mut Ctx) -> Transition {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Transition::Pop,
            _ => Transition::None,
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("q", "back")]
    }
}
