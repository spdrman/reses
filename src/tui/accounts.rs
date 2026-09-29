//! Pick an AWS profile from the credentials file, or add one.
//!
//! I read the profiles straight out of `~/.aws/credentials` (the same file the aws CLI uses)
//! and list them with their region and key ID. Enter connects one and pushes the bucket
//! browser on it; `a` opens a small form that validates a new profile and writes it back into
//! that file. I keep secrets masked by default and out of every error message, because this
//! screen is the one place reses ever handles a raw secret key. I'm the first screen when no
//! account is connected yet, and the inbox can push me again with `u` to switch accounts.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::browser::BrowserScreen;
use super::{Ctx, Session, Transition, View};
use crate::aws_profile::{self, Profile};

type Connector = Box<dyn Fn(Profile) -> Session>;

pub struct AccountsScreen {
    profiles: Vec<Profile>,
    /// Region shown for each profile: its own, else the one in ~/.aws/config.
    regions: Vec<Option<String>>,
    file_exists: bool,
    /// Root accounts screen is allowed to follow the selected session. When pushed over another
    /// view it keeps the existing context until a new inbox is saved.
    root: bool,
    selected: usize,
    form: Option<AccountForm>,
    connect: Connector,
}

impl AccountsScreen {
    /// Build the first screen or an account picker over the current session. On the first
    /// screen, q quits; when pushed over another view, q goes back.
    pub fn new(ctx: &mut Ctx) -> Self {
        let mut screen = Self {
            profiles: Vec::new(),
            regions: Vec::new(),
            file_exists: false,
            root: ctx.session.is_none(),
            selected: 0,
            form: None,
            connect: Box::new(|p| Session::connect(p, None)),
        };
        screen.reload(ctx);
        // Start on the default when there is one.
        if let Some(i) = ctx
            .config
            .default_profile
            .as_ref()
            .and_then(|d| screen.profiles.iter().position(|p| &p.name == d))
        {
            screen.selected = i;
        }
        screen
    }
    /// The settings picker is always a child view, even before an account is connected.
    pub(crate) fn from_settings(ctx: &mut Ctx) -> Self {
        let mut screen = Self::new(ctx);
        screen.root = false;
        screen
    }

    /// Connect through `f` instead of real S3, so tests can hand out a `MemoryStore`.
    #[cfg(test)]
    pub(crate) fn with_connector(mut self, f: impl Fn(Profile) -> Session + 'static) -> Self {
        self.connect = Box::new(f);
        self
    }

    /// I re-read the credentials file and work out each profile's region, then clamp the
    /// selection so a profile that vanished doesn't leave it pointing past the end.
    fn reload(&mut self, ctx: &mut Ctx) {
        self.file_exists = ctx.creds_path.exists();
        self.profiles = ctx.credentials().map(|f| f.profiles()).unwrap_or_default();
        self.regions = self
            .profiles
            .iter()
            .map(|p| {
                p.region
                    .clone()
                    .or_else(|| aws_profile::region_from_config(&p.name))
            })
            .collect();
        self.selected = self.selected.min(self.profiles.len().saturating_sub(1));
    }

    /// I label q as quit on the first screen and as back when I'm pushed over another one.
    fn q_hint(&self) -> (&'static str, &'static str) {
        if self.root {
            ("q", "quit")
        } else {
            ("q", "back")
        }
    }

    /// I connect the profile and push a browser on that session. Only as the root screen do I
    /// make it the shared session too.
    fn open_browser(&self, profile: Profile, ctx: &mut Ctx) -> Transition {
        let session = (self.connect)(profile);
        // As the first screen nothing below uses ctx.session, so it can follow this account and
        // the header bar shows it. Pushed over the inbox, the inbox keeps its own.
        if self.root {
            ctx.session = Some(session.clone());
        }
        Transition::Push(Box::new(BrowserScreen::with_session(session)))
    }

    /// I handle a key on the profile list: move, connect, add, reload, or make a profile the
    /// default.
    fn list_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        let n = self.profiles.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Transition::Pop,
            KeyCode::Down | KeyCode::Char('j') if n > 0 => {
                self.selected = (self.selected + 1).min(n - 1);
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.selected = n.saturating_sub(1),
            KeyCode::Char('a') => self.form = Some(AccountForm::default()),
            KeyCode::Char('r') => self.reload(ctx),
            KeyCode::Enter => {
                if let Some(p) = self.profiles.get(self.selected).cloned() {
                    return self.open_browser(p, ctx);
                }
            }
            KeyCode::Char('d') => {
                if let Some(p) = self.profiles.get(self.selected) {
                    let name = p.name.clone();
                    ctx.config.default_profile = Some(name.clone());
                    if ctx.save_config() {
                        ctx.info(format!("{name} is now the default account"));
                    }
                }
            }
            _ => {}
        }
        Transition::None
    }

    /// I pass a key to the open form and act on what it asks for: stay, cancel, or save.
    fn form_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        let Some(form) = self.form.as_mut() else {
            return Transition::None;
        };
        match form.on_key(key) {
            FormAction::Continue => Transition::None,
            FormAction::Cancel => {
                self.form = None;
                Transition::None
            }
            FormAction::Save { overwrite } => self.save_form(overwrite, ctx),
        }
    }

    /// I validate the form, ask before writing keys into a section that already exists, write
    /// the credentials file, and connect the new profile. Anything that fails stays on the form
    /// with the reason on the status line.
    fn save_form(&mut self, overwrite: bool, ctx: &mut Ctx) -> Transition {
        let Some(form) = self.form.as_mut() else {
            return Transition::None;
        };
        let profile = match form.profile() {
            Ok(p) => p,
            Err(msg) => {
                ctx.error(msg);
                return Transition::None;
            }
        };
        let Some(mut file) = ctx.credentials() else {
            return Transition::None;
        };
        // Any section of that name counts, even one with only a region or role_arn, since
        // writing keys into it changes something the user already has.
        if !overwrite && (file.has_section(&profile.name) || file.get(&profile.name).is_some()) {
            form.confirm_overwrite = true;
            return Transition::None;
        }
        if let Err(e) = file.upsert(&profile).and_then(|()| file.save()) {
            ctx.error(e.to_string());
            return Transition::None;
        }
        if ctx.config.default_profile.is_none() {
            ctx.config.default_profile = Some(profile.name.clone());
            if !ctx.save_config() {
                return Transition::None;
            }
        }
        self.form = None;
        self.reload(ctx);
        if let Some(i) = self.profiles.iter().position(|p| p.name == profile.name) {
            self.selected = i;
        }
        self.open_browser(profile, ctx)
    }

    /// I draw the profile list with aligned name, region and key ID columns, or a hint about
    /// adding an account when there are no profiles yet.
    fn render_list(&self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let [head, body] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " AWS profiles in ",
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::raw(ctx.creds_path.display().to_string()),
            ])),
            head,
        );

        if self.profiles.is_empty() {
            let msg = if self.file_exists {
                format!(
                    " No profiles in {} yet, press a to add an account.",
                    ctx.creds_path.display()
                )
            } else {
                format!(
                    " {} does not exist yet, press a to add an account.",
                    ctx.creds_path.display()
                )
            };
            frame.render_widget(
                Paragraph::new(msg)
                    .style(Style::default().add_modifier(Modifier::BOLD))
                    .wrap(Wrap { trim: false }),
                body,
            );
            return;
        }

        let name_w = self
            .profiles
            .iter()
            .map(|p| p.name.chars().count())
            .max()
            .unwrap_or(0)
            .max(8);
        let region_w = self
            .regions
            .iter()
            .map(|r| r.as_deref().map_or(1, |r| r.len()))
            .max()
            .unwrap_or(0)
            .max(6);
        let default = ctx.config.default_profile.as_deref();
        let items: Vec<ListItem> = self
            .profiles
            .iter()
            .zip(&self.regions)
            .map(|(p, region)| {
                let mut spans = vec![
                    Span::styled(
                        format!("{:<name_w$}  ", super::text::escape(&p.name)),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("{:<region_w$}  ", region.as_deref().unwrap_or("-"))),
                    Span::styled(
                        format!("{:<20}", super::text::escape(&p.access_key_id)),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ];
                if Some(p.name.as_str()) == default {
                    spans.push(Span::styled(
                        "  (default)",
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("> ")
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            body,
            &mut state,
        );
    }
}

impl View for AccountsScreen {
    /// I title the screen after whichever of the list or the form is showing.
    fn title(&self) -> String {
        if self.form.is_some() {
            "Add account".into()
        } else {
            "Accounts".into()
        }
    }

    /// I draw the form when it's open and the list otherwise.
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        match &self.form {
            Some(form) => form.render(frame, area, ctx),
            None => self.render_list(frame, area, ctx),
        }
    }

    /// I route a key to the form when it's open and to the list otherwise.
    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition {
        if self.form.is_some() {
            self.form_key(key, ctx)
        } else {
            self.list_key(key, ctx)
        }
    }

    /// I reload the profiles whenever I come back to the top, since the form or another tool
    /// may have changed the file.
    fn on_focus(&mut self, ctx: &mut Ctx) {
        self.reload(ctx);
    }

    /// A pasted key goes into the field being edited, flattened to one line: copied keys often
    /// bring a newline, and a paste must never press Enter or Tab. Outside the form it's ignored.
    fn on_paste(&mut self, text: &str, _ctx: &mut Ctx) -> Transition {
        if let Some(form) = self.form.as_mut()
            && !form.confirm_overwrite
        {
            form.values[form.focus].extend(text.chars().filter(|c| !c.is_control()));
        }
        Transition::None
    }

    /// I show the keys that do something in the current mode: the overwrite prompt, the form,
    /// or the list.
    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.form {
            Some(f) if f.confirm_overwrite => vec![("y", "overwrite"), ("any key", "back")],
            Some(_) => vec![
                ("tab", "next"),
                ("ctrl-s", "save"),
                ("ctrl-r", "reveal"),
                ("esc", "cancel"),
            ],
            None if self.profiles.is_empty() => vec![("a", "add account"), self.q_hint()],
            None => vec![
                ("enter", "connect"),
                ("a", "add"),
                ("d", "make default"),
                ("r", "reload"),
                self.q_hint(),
            ],
        }
    }

    /// While the add-account form is open, a `?` is part of what's being typed.
    fn taking_text(&self) -> bool {
        self.form.is_some()
    }
}

// ---- the add-account form ----

const MASK: &str = "********";
const NAME: usize = 0;
const KEY_ID: usize = 1;
const SECRET: usize = 2;
const TOKEN: usize = 3;
const REGION: usize = 4;
const LABELS: [&str; 5] = [
    "Profile name",
    "Access key ID",
    "Secret access key",
    "Session token",
    "Region",
];
const PLACEHOLDERS: [&str; 5] = [
    "e.g. work",
    "AKIA...",
    "",
    "optional, only for temporary credentials",
    "optional, e.g. eu-west-1",
];

#[derive(Default)]
struct AccountForm {
    values: [String; 5],
    focus: usize,
    reveal: bool,
    confirm_overwrite: bool,
}

enum FormAction {
    Continue,
    Cancel,
    Save { overwrite: bool },
}

impl AccountForm {
    /// I handle a key in the form. While the overwrite prompt is up only y saves; otherwise I
    /// type into the focused field, move focus, toggle the reveal, or ask to save or cancel.
    fn on_key(&mut self, key: KeyEvent) -> FormAction {
        if self.confirm_overwrite {
            self.confirm_overwrite = false;
            return if key.code == KeyCode::Char('y') {
                FormAction::Save { overwrite: true }
            } else {
                FormAction::Continue
            };
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return FormAction::Cancel,
            KeyCode::Char('s') if ctrl => return FormAction::Save { overwrite: false },
            KeyCode::Char('r') if ctrl => self.reveal = !self.reveal,
            // Any other ctrl or alt chord is a command, not text for the field.
            KeyCode::Char(_) if ctrl || key.modifiers.contains(KeyModifiers::ALT) => {}
            KeyCode::Char(c) => self.values[self.focus].push(c),
            KeyCode::Backspace => {
                self.values[self.focus].pop();
            }
            KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % LABELS.len(),
            KeyCode::BackTab | KeyCode::Up => {
                self.focus = (self.focus + LABELS.len() - 1) % LABELS.len();
            }
            KeyCode::Enter if self.focus == LABELS.len() - 1 => {
                return FormAction::Save { overwrite: false };
            }
            KeyCode::Enter => self.focus += 1,
            _ => {}
        }
        FormAction::Continue
    }

    /// Validate and build the profile. Error messages never include a secret.
    fn profile(&self) -> Result<Profile, String> {
        let v = |i: usize| self.values[i].trim().to_string();
        let name = v(NAME);
        if name.is_empty() {
            return Err("profile name is required".into());
        }
        if name.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '[' | ']' | '=' | '#' | ';')
        }) {
            return Err(format!(
                "profile name {name:?} can't contain spaces, brackets, =, # or ;"
            ));
        }
        let key_id = v(KEY_ID);
        if key_id.is_empty() {
            return Err("access key ID is required".into());
        }
        if !(16..=128).contains(&key_id.len()) || !key_id.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return Err(
                "access key ID should be 16 to 128 letters and digits (like AKIA...)".into(),
            );
        }
        let secret = v(SECRET);
        if secret.is_empty() {
            return Err("secret access key is required".into());
        }
        if secret.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("secret access key can't contain spaces".into());
        }
        let token = v(TOKEN);
        if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("session token can't contain spaces".into());
        }
        let region = v(REGION);
        if !region.is_empty() && !valid_region(&region) {
            return Err(format!(
                "region {region:?} doesn't look like an AWS region (e.g. us-east-1)"
            ));
        }
        Ok(Profile {
            name,
            access_key_id: key_id,
            secret_access_key: secret,
            session_token: (!token.is_empty()).then_some(token),
            region: (!region.is_empty()).then_some(region),
        })
    }

    /// I draw each field with its label, a fixed width mask over secrets unless revealed, and
    /// placeholders for empty fields, then the overwrite question or a note on where keys go.
    fn render(&self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        // DIM rather than dark grey, which vanished on Solarized Dark.
        let dim = Style::default().add_modifier(Modifier::DIM);
        let mut lines = vec![
            Line::styled(
                format!(" New profile for {}", ctx.creds_path.display()),
                dim,
            ),
            Line::raw(""),
        ];
        let label_w = LABELS.iter().map(|l| l.len()).max().unwrap_or(0);
        for (i, label) in LABELS.iter().enumerate() {
            let focused = i == self.focus;
            let value = &self.values[i];
            let masked = (i == SECRET || i == TOKEN) && !self.reveal;
            let shown = if masked {
                // Fixed width, so the mask says a value is there without saying how long it is.
                if value.is_empty() {
                    String::new()
                } else {
                    MASK.to_string()
                }
            } else {
                value.clone()
            };
            let marker = if focused { "> " } else { "  " };
            let label_style = if focused {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(format!("{marker}{label:<label_w$}  "), label_style),
                Span::styled(
                    shown,
                    if focused {
                        Style::default().add_modifier(Modifier::UNDERLINED)
                    } else {
                        Style::default()
                    },
                ),
            ];
            if focused {
                spans.push(Span::styled(
                    "_",
                    Style::default().add_modifier(Modifier::SLOW_BLINK),
                ));
            }
            if value.is_empty() && !PLACEHOLDERS[i].is_empty() {
                spans.push(Span::styled(format!(" {}", PLACEHOLDERS[i]), dim));
            }
            if masked && !value.is_empty() && focused {
                spans.push(Span::styled("  (ctrl-r shows it)", dim));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::raw(""));
        if self.confirm_overwrite {
            let name = self.values[NAME].trim();
            lines.push(Line::styled(
                format!(
                    " Profile '{name}' already exists in {}. Overwrite its keys? Press y to overwrite, any other key to go back.",
                    ctx.creds_path.display()
                ),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        } else {
            lines.push(Line::styled(
                " The keys are written to the AWS credentials file, the same place the aws CLI reads.",
                dim,
            ));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
}

/// Loose check for names like us-east-1, eu-central-2, us-gov-west-1, cn-north-1.
fn valid_region(r: &str) -> bool {
    let parts: Vec<&str> = r.split('-').collect();
    parts.len() >= 3
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
        && parts
            .last()
            .is_some_and(|p| p.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests;
