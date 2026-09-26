//! One decoded message, fetched from S3.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use super::{Ctx, Transition, View};

pub struct MessageScreen {
    pub bucket: String,
    pub key: String,
}

impl MessageScreen {
    pub fn new(bucket: String, key: String) -> Self {
        Self { bucket, key }
    }
}

impl View for MessageScreen {
    fn title(&self) -> String {
        "Message".into()
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        frame.render_widget(Paragraph::new("Message: not built yet"), area);
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
