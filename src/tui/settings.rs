//! Select the saved inbox and the directory used for private opened-message copies.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use super::accounts::AccountsScreen;
use super::{Ctx, Transition, View, text};

pub(crate) struct SettingsScreen {
    selected: usize,
    editing_dir: Option<String>,
    confirming_clear: bool,
}

impl SettingsScreen {
    pub(crate) fn new() -> Self {
        Self {
            selected: 0,
            editing_dir: None,
            confirming_clear: false,
        }
    }

    /// Save a usable parent for future private message copies. Keep the old path on any failure.
    fn save_dir(&mut self, next: Option<PathBuf>, ctx: &mut Ctx) -> bool {
        if let Some(dir) = &next {
            if !dir.is_absolute() {
                ctx.error("enter an absolute directory path");
                return false;
            }
            if let Err(e) = tempfile::Builder::new()
                .prefix("reSES-check-")
                .tempdir_in(dir)
            {
                ctx.error(format!(
                    "cannot create private files in {}: {e}",
                    dir.display()
                ));
                return false;
            }
        }
        let previous = std::mem::replace(&mut ctx.config.temp_dir, next);
        if !ctx.save_config() {
            ctx.config.temp_dir = previous;
            return false;
        }
        ctx.page_dir = ctx
            .config
            .temp_dir
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        ctx.info("Message temporary directory saved.");
        true
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
        let chosen = Style::default().add_modifier(Modifier::REVERSED);
        let inbox_style = if self.selected == 0 {
            chosen
        } else {
            Style::default()
        };
        let dir_style = if self.selected == 1 {
            chosen
        } else {
            Style::default()
        };
        let dir = match &self.editing_dir {
            Some(input) => format!("{}_", text::escape(input)),
            None => format!(
                "{}{}",
                text::escape(&ctx.page_dir.to_string_lossy()),
                if ctx.config.temp_dir.is_none() {
                    " (OS default)"
                } else {
                    ""
                }
            ),
        };
        let instruction = if self.confirming_clear {
            "Clear the saved inbox? y confirms; n or Esc cancels."
        } else if self.editing_dir.is_some() {
            "Type or paste an absolute path; empty uses the OS default. Enter saves; Esc cancels."
        } else if self.selected == 0 {
            "Enter picks an account and S3 folder; c clears the saved inbox."
        } else {
            "Enter edits the directory; r restores the OS default."
        };
        let lines = vec![
            Line::styled("  Saved inbox", inbox_style),
            Line::styled(format!("  {inbox}"), inbox_style),
            Line::raw(""),
            Line::styled("  Message temporary directory", dir_style),
            Line::styled(format!("  {dir}"), dir_style),
            Line::raw(""),
            Line::raw(instruction),
            Line::raw("Opened messages use private per-message directories here."),
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
        if let Some(input) = self.editing_dir.as_mut() {
            match key.code {
                KeyCode::Esc => self.editing_dir = None,
                KeyCode::Enter => {
                    let value = std::mem::take(input);
                    let path = if value.is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(&value))
                    };
                    if self.save_dir(path, ctx) {
                        self.editing_dir = None;
                    } else {
                        self.editing_dir = Some(value);
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.clear()
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    input.push(c)
                }
                _ => {}
            }
            return Transition::None;
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => self.selected = (self.selected + 1).min(1),
            KeyCode::Enter if self.selected == 0 => {
                return Transition::Push(Box::new(AccountsScreen::from_settings(ctx)));
            }
            KeyCode::Enter => {
                self.editing_dir = Some(
                    ctx.config
                        .temp_dir
                        .as_ref()
                        .map_or_else(String::new, |p| p.to_string_lossy().into_owned()),
                );
            }
            KeyCode::Char('c')
                if key.modifiers.is_empty() && self.selected == 0 && ctx.config.inbox.is_some() =>
            {
                self.confirming_clear = true;
            }
            KeyCode::Char('r') if key.modifiers.is_empty() && self.selected == 1 => {
                self.save_dir(None, ctx);
            }
            KeyCode::Esc | KeyCode::Char('q') => return Transition::Pop,
            _ => {}
        }
        Transition::None
    }

    fn on_paste(&mut self, pasted: &str, _ctx: &mut Ctx) -> Transition {
        if let Some(input) = self.editing_dir.as_mut() {
            input.extend(pasted.chars().filter(|c| !c.is_control()));
        }
        Transition::None
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.confirming_clear {
            vec![("y", "clear saved inbox"), ("n / esc", "cancel")]
        } else if self.editing_dir.is_some() {
            vec![
                ("enter", "save path"),
                ("ctrl-u", "clear input"),
                ("esc", "cancel"),
            ]
        } else {
            vec![
                ("↑↓ / tab", "select"),
                ("enter", "edit"),
                ("c", "clear inbox"),
                ("r", "OS default"),
                ("esc", "back"),
            ]
        }
    }

    fn taking_text(&self) -> bool {
        self.editing_dir.is_some()
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
    }

    /// Enter on the inbox launches the account/folder picker; q returns without changing it.
    #[test]
    fn selecting_inbox_opens_account_picker() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::tui::testing::ctx(dir.path(), None);
        let mut app = App::with_view(ctx, Box::new(SettingsScreen::new()));
        app.key(key(KeyCode::Enter));
        assert_eq!(app.stack.last().unwrap().title(), "Accounts");
        app.key(key(KeyCode::Char('q')));
        assert_eq!(app.stack.last().unwrap().title(), "Settings");
        assert_eq!(app.ctx.config.inbox, None);
    }

    /// Selecting a writable path changes where opened messages go now and after a restart.
    #[test]
    fn temporary_directory_input_changes_message_copy_location_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let chosen = dir.path().join("mail copies");
        std::fs::create_dir(&chosen).unwrap();
        let ctx = crate::tui::testing::ctx(dir.path(), None);
        let mut app = App::with_view(ctx, Box::new(SettingsScreen::new()));

        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Enter));
        app.paste(&chosen.to_string_lossy());
        app.key(key(KeyCode::Enter));
        assert_eq!(app.ctx.config.temp_dir, Some(chosen.clone()));
        assert_eq!(app.ctx.page_dir, chosen);
        assert_eq!(
            AppConfig::load(&app.ctx.config_path).unwrap().temp_dir,
            app.ctx.config.temp_dir
        );
        let (copy, message) =
            crate::tui::page::write(&app.ctx.page_dir, b"Subject: Hello\n\nbody").unwrap();
        assert!(message.starts_with(&chosen));
        assert_eq!(std::fs::read(message).unwrap(), b"Subject: Hello\n\nbody");
        drop(copy);

        let loaded = AppConfig::load(&app.ctx.config_path).unwrap();
        let restarted = crate::tui::Ctx::new(
            loaded,
            app.ctx.config_path.clone(),
            app.ctx.creds_path.clone(),
            crate::tui::jobs::Jobs::inline(),
        );
        assert_eq!(restarted.page_dir, chosen);
        app.key(key(KeyCode::Char('r')));
        assert_eq!(app.ctx.config.temp_dir, None);
        assert_eq!(app.ctx.page_dir, std::env::temp_dir());
        assert_eq!(
            AppConfig::load(&app.ctx.config_path).unwrap().temp_dir,
            None
        );
    }

    /// Invalid or unsavable choices leave the active and persisted location alone.
    #[test]
    fn temporary_directory_rejects_bad_path_and_failed_save() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::tui::testing::ctx(dir.path(), None);
        let original = ctx.page_dir.clone();
        let mut app = App::with_view(ctx, Box::new(SettingsScreen::new()));
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Enter));
        app.paste("relative/path\n");
        app.key(key(KeyCode::Enter));
        assert_eq!(app.ctx.page_dir, original);
        assert!(matches!(app.ctx.status, Some(Status::Error(_))));

        app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let missing = dir.path().join("missing");
        app.paste(&missing.to_string_lossy());
        app.key(key(KeyCode::Enter));
        assert_eq!(app.ctx.page_dir, original);
        assert_eq!(app.ctx.config.temp_dir, None);

        app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        app.paste(&dir.path().to_string_lossy());
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, b"x").unwrap();
        let real_config_path = app.ctx.config_path.clone();
        app.ctx.config_path = blocker.join("config.toml");
        app.key(key(KeyCode::Enter));
        assert_eq!(app.ctx.page_dir, original);
        assert_eq!(app.ctx.config.temp_dir, None);
        assert_eq!(
            AppConfig::load(&real_config_path).unwrap(),
            AppConfig::default()
        );
    }

    /// Esc discards edits and text pasted into an input cannot trigger navigation.
    #[test]
    fn temporary_directory_cancel_preserves_previous_choice() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::tui::testing::ctx(dir.path(), None);
        let mut app = App::with_view(ctx, Box::new(SettingsScreen::new()));
        app.key(key(KeyCode::Tab));
        app.key(key(KeyCode::Enter));
        app.paste("s?c\n");
        assert_eq!(app.stack.len(), 1);
        app.key(key(KeyCode::Esc));
        assert_eq!(app.ctx.page_dir, dir.path());
        assert_eq!(app.ctx.config.temp_dir, None);
        app.key(key(KeyCode::Esc));
        assert!(app.quit);
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
