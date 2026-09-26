//! Pick an AWS profile from the credentials file, or add one.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use super::{Ctx, Transition, View};

pub struct AccountsScreen {}

impl AccountsScreen {
    pub fn new(ctx: &mut Ctx) -> Self {
        let _ = ctx;
        Self {}
    }

    #[cfg(test)]
    pub(crate) fn with_connector(self, f: impl Fn(crate::aws_profile::Profile) -> super::Session + 'static) -> Self {
        let _ = f;
        self
    }
}

#[cfg(test)]
mod tests;

impl View for AccountsScreen {
    fn title(&self) -> String {
        "Accounts".into()
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, _ctx: &Ctx) {
        frame.render_widget(Paragraph::new("Accounts: not built yet"), area);
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
