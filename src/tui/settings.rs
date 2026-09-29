//! Saved inbox settings and a read-only reminder of the OS temporary directory.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use super::{Ctx, Transition, View, text};

pub(crate) struct SettingsScreen {
    confirming_clear: bool,
    temp_dir: String,
}

impl SettingsScreen {
    pub(crate) fn new() -> Self {
        Self {
            confirming_clear: false,
            temp_dir: text::escape(&std::env::temp_dir().to_string_lossy()).into_owned(),
        }
    }
}

impl View for SettingsScreen {
    fn title(&self) -> String {
        "Settings".into()
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let inbox = match &ctx.config.inbox {
            Some(inbox) => format!(
                "{} / {} / {}",
                text::escape(&inbox.profile),
                text::escape(&inbox.bucket),
                if inbox.prefix.is_empty() {
                    "(bucket root)".to_string()
                } else {
                    text::escape(&inbox.prefix).into_owned()
                }
            ),
            None => "No inbox is saved".into(),
        };

        let lines = vec![
            Line::raw("Saved inbox:"),
            Line::raw(inbox),
            Line::raw("Change the saved inbox from the S3 browser with i."),
            Line::raw(""),
            Line::raw("OS temporary directory (read-only):"),
            Line::raw(self.temp_dir.as_str()),
            Line::raw("Opened messages use private per-message temporary directories."),
            Line::raw(""),
            Line::raw(if self.confirming_clear {
                "Clear the saved inbox? y confirms; n or Esc cancels."
            } else if ctx.config.inbox.is_some() {
                "Press c to clear the saved inbox (does not delete S3 mail)."
            } else {
                "No saved inbox to clear."
            }),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        if self.confirming_clear {
            match key.code {
                KeyCode::Char('y') if key.modifiers.is_empty() => {
                    self.confirming_clear = false;
                    let previous = ctx.config.inbox.take();
                    if !ctx.save_config() {
                        ctx.config.inbox = previous;
                    } else {
                        ctx.info("Saved inbox cleared. S3 mail was not deleted.");
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') => self.confirming_clear = false,
                _ => {}
            }
            return Transition::None;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.is_empty() && ctx.config.inbox.is_some() => {
                self.confirming_clear = true;
                Transition::None
            }
            KeyCode::Esc | KeyCode::Char('q') => Transition::Pop,
            _ => Transition::None,
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.confirming_clear {
            vec![("y", "clear saved inbox"), ("n / esc", "cancel")]
        } else {
            vec![("c", "clear saved inbox"), ("esc", "back")]
        }
    }

    fn is_settings(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyModifiers;

    use super::*;
    use crate::config::{AppConfig, Inbox};
    use crate::tui::testing::{key, screen};
    use crate::tui::{App, Status};

    fn saved_inbox() -> Inbox {
        Inbox {
            profile: "work".into(),
            bucket: "mail-bucket".into(),
            prefix: "inbound/".into(),
            region: Some("us-east-1".into()),
        }
    }

    /// Typing s into the real browser filter does not open Settings. Outside the filter,
    /// the same key opens it, and Esc returns to the browser.
    #[test]
    fn settings_shortcut_respects_browser_filter_and_returns() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::tui::testing::ctx(
            dir.path(),
            Some(std::sync::Arc::new(crate::s3::MemoryStore::new())),
        );
        let mut app = App::with_view(ctx, Box::new(crate::tui::browser::BrowserScreen::new()));
        crate::tui::testing::settle(&mut app);
        app.key(key(KeyCode::Char('/')));
        app.key(key(KeyCode::Char('s')));
        assert_eq!(app.stack.len(), 1);
        assert!(screen(&mut app, 100, 20).contains("/s_"));

        app.key(key(KeyCode::Esc));
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(app.stack.len(), 1, "Ctrl-s is not the bare shortcut");
        app.key(key(KeyCode::Char('s')));
        assert_eq!(app.stack.last().unwrap().title(), "Settings");
        let displayed = screen(&mut app, 120, 20);
        assert!(displayed.contains("OS temporary directory (read-only)"));
        let os_path = text::escape(&std::env::temp_dir().to_string_lossy()).into_owned();
        assert!(displayed.contains(&os_path), "{displayed}");
        app.key(key(KeyCode::Char('s')));
        assert_eq!(app.stack.len(), 2, "Settings must not stack on itself");
        app.key(key(KeyCode::Esc));
        assert_eq!(app.stack.last().unwrap().title(), "Browse S3");
        assert!(!app.quit);
    }

    /// Clearing needs explicit confirmation and changes the config file, not any S3 message.
    #[test]
    fn clearing_inbox_requires_confirmation_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = crate::tui::testing::ctx(dir.path(), None);
        ctx.config.inbox = Some(saved_inbox());
        ctx.config.save(&ctx.config_path).unwrap();
        let mut app = App::with_view(ctx, Box::new(SettingsScreen::new()));
        let displayed = screen(&mut app, 100, 20);
        assert!(displayed.contains("mail-bucket / inbound/"), "{displayed}");
        assert!(displayed.contains("browser with i"), "{displayed}");

        app.key(key(KeyCode::Char('c')));
        app.key(key(KeyCode::Esc));
        assert_eq!(app.ctx.config.inbox, Some(saved_inbox()));
        assert_eq!(
            AppConfig::load(&app.ctx.config_path).unwrap().inbox,
            Some(saved_inbox())
        );
        app.key(key(KeyCode::Char('c')));
        app.key(key(KeyCode::Char('y')));
        assert_eq!(app.ctx.config.inbox, None);
        assert_eq!(AppConfig::load(&app.ctx.config_path).unwrap().inbox, None);
        assert!(matches!(app.ctx.status, Some(Status::Info(_))));
        assert!(screen(&mut app, 100, 20).contains("No inbox is saved"));
    }

    /// An unwritable settings location leaves the old inbox in memory and on disk.
    #[test]
    fn failed_save_restores_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = crate::tui::testing::ctx(dir.path(), None);
        ctx.config.inbox = Some(saved_inbox());
        ctx.config.save(&ctx.config_path).unwrap();
        let real_config_path = ctx.config_path.clone();
        let file = dir.path().join("not-a-directory");
        std::fs::write(&file, "untouched").unwrap();
        ctx.config_path = file.join("config.toml");
        let mut app = App::with_view(ctx, Box::new(SettingsScreen::new()));
        app.key(key(KeyCode::Char('c')));
        app.key(key(KeyCode::Char('y')));

        assert_eq!(app.ctx.config.inbox, Some(saved_inbox()));
        assert_eq!(
            AppConfig::load(&real_config_path).unwrap().inbox,
            Some(saved_inbox())
        );
        assert!(matches!(app.ctx.status, Some(Status::Error(_))));
    }
}
