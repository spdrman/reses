use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::{Ctx, Start, Transition, View};

pub(crate) struct SettingsScreen {
    forced: bool,
    resume: Option<Start>,
    editing: bool,
    input: String,
}

impl SettingsScreen {
    /// I require a usable alternative when the platform cache failed its startup check.
    pub(crate) fn new(forced: bool, resume: Option<Start>) -> Self {
        Self {
            forced,
            resume,
            editing: false,
            input: String::new(),
        }
    }

    /// I validate and persist the alternative before I let the app continue.
    fn save_temp_dir(&mut self, ctx: &mut Ctx) -> bool {
        let path = PathBuf::from(self.input.trim());
        if !path.is_absolute() {
            ctx.error("The temporary directory path must be absolute.");
            return false;
        }
        if let Err(error) = super::page::check_cache_dir(&path) {
            ctx.error(format!("Cannot use {}: {error}", path.display()));
            return false;
        }
        let old = ctx.config.temp_dir.replace(path.clone());
        if !ctx.save_config() {
            ctx.config.temp_dir = old;
            return false;
        }
        ctx.page_dir = path;
        ctx.cache_error = None;
        self.forced = false;
        self.editing = false;
        ctx.info("Temporary directory saved.");
        true
    }
}

impl View for SettingsScreen {
    fn title(&self) -> String {
        "Settings".to_string()
    }

    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let [body, footer] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(area);
        let inbox = match &ctx.config.inbox {
            Some(inbox) => format!(
                "{} / {} / {}",
                inbox.profile,
                inbox.bucket,
                if inbox.prefix.is_empty() {
                    "(bucket root)"
                } else {
                    &inbox.prefix
                }
            ),
            None => "No inbox is saved".to_string(),
        };
        let temp_dir = if self.editing {
            self.input.clone()
        } else {
            ctx.page_dir.display().to_string()
        };
        let mut lines = vec![
            Line::from(Span::raw("Saved inbox: ")),
            Line::from(Span::raw(inbox)),
            Line::raw(""),
            Line::from(Span::raw("Temporary files directory:")),
            Line::from(Span::raw(temp_dir)),
        ];
        if self.forced {
            lines.push(Line::raw(""));
            lines.push(Line::raw(
                "The cache directory is unusable. Set a writable alternative to continue.",
            ));
        }
        if self.editing {
            lines.push(Line::raw(""));
            lines.push(Line::raw(
                "Type an absolute directory path, then press Enter to validate and save.",
            ));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
        let footer_text = if self.editing {
            " Enter save   Esc cancel "
        } else {
            " t change temporary directory   Esc back "
        };
        frame.render_widget(Paragraph::new(footer_text), footer);
    }

    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        if self.editing {
            match key.code {
                KeyCode::Esc => {
                    self.editing = false;
                    self.input.clear();
                }
                KeyCode::Enter => {
                    if self.save_temp_dir(ctx)
                        && let Some(start) = self.resume.take()
                    {
                        return Transition::Reset(super::initial_view(ctx, start));
                    }
                }
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(c) => self.input.push(c),
                _ => {}
            }
            return Transition::None;
        }
        match key.code {
            KeyCode::Char('t') => {
                self.input.clear();
                self.editing = true;
            }
            KeyCode::Esc | KeyCode::Char('q') if !self.forced => return Transition::Pop,
            _ => {}
        }
        Transition::None
    }

    fn on_paste(&mut self, text: &str, _ctx: &mut Ctx) -> Transition {
        if self.editing {
            self.input.push_str(text);
        }
        Transition::None
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.editing {
            vec![("enter", "save"), ("esc", "cancel")]
        } else {
            vec![("t", "temporary directory"), ("esc", "back")]
        }
    }

    fn taking_text(&self) -> bool {
        self.editing
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    use super::*;

    /// I refuse to save a path that cannot be used as a directory.
    #[test]
    fn an_unusable_temp_dir_is_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("file");
        std::fs::write(&blocked, b"file").unwrap();
        let mut ctx = crate::tui::testing::ctx(dir.path(), None);
        ctx.cache_error = Some("unavailable".to_string());
        let mut screen = SettingsScreen::new(true, Some(Start::Accounts));
        screen.editing = true;
        screen.input = blocked.display().to_string();

        screen.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut ctx);

        assert_eq!(ctx.config.temp_dir, None);
        assert!(screen.editing);
        assert!(!ctx.config_path.exists());
    }

    /// I persist only a directory that passed the real read/write check.
    #[test]
    fn a_usable_temp_dir_is_saved_and_resumes_startup() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("alternate");
        let mut ctx = crate::tui::testing::ctx(dir.path(), None);
        ctx.cache_error = Some("unavailable".to_string());
        let mut screen = SettingsScreen::new(true, Some(Start::Accounts));
        screen.editing = true;
        screen.input = cache.display().to_string();

        let transition = screen.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut ctx);

        assert_eq!(ctx.config.temp_dir.as_deref(), Some(cache.as_path()));
        assert_eq!(ctx.page_dir, cache);
        assert!(ctx.cache_error.is_none());
        assert!(matches!(transition, Transition::Reset(_)));
        assert!(ctx.config_path.exists());
    }
    /// I open Settings from a browser and show the inbox location I have saved.
    #[test]
    fn settings_shortcut_shows_saved_inbox_location() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = crate::tui::testing::ctx(dir.path(), None);
        ctx.config.inbox = Some(crate::config::Inbox {
            profile: "work".to_string(),
            bucket: "mail-bucket".to_string(),
            prefix: "inbound/".to_string(),
            region: Some("us-east-1".to_string()),
        });
        let mut app =
            crate::tui::App::with_view(ctx, Box::new(super::super::browser::BrowserScreen::new()));
        app.key(crate::tui::testing::key(KeyCode::Char('s')));

        assert_eq!(app.stack.last().unwrap().title(), "Settings");
        let screen = crate::tui::testing::screen(&mut app, 100, 20);
        assert!(screen.contains("mail-bucket"), "{screen}");
        assert!(screen.contains("inbound/"), "{screen}");
    }
    /// I keep ordinary path characters in the editor and cancel without leaving Settings.
    #[test]
    fn path_editor_handles_typed_t_and_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = crate::tui::testing::ctx(dir.path(), None);
        let mut screen = SettingsScreen::new(false, None);
        screen.editing = true;
        screen.input = "/tmp/cache".to_string();

        screen.on_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            &mut ctx,
        );
        assert_eq!(screen.input, "/tmp/cachet");
        assert!(screen.editing);

        assert!(matches!(
            screen.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut ctx),
            Transition::None
        ));
        assert!(!screen.editing);
    }
}
