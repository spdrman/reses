//! The inbox list: one row per stored message, From / Subject / Date / Size.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use super::{Ctx, Transition, View};
use crate::config::Inbox;

pub struct InboxScreen {
    pub inbox: Inbox,
}

impl InboxScreen {
    pub fn new(inbox: Inbox) -> Self {
        Self { inbox }
    }
}

impl View for InboxScreen {
    fn title(&self) -> String {
        "Inbox".into()
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        frame.render_widget(Paragraph::new("Inbox: not built yet"), area);
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
