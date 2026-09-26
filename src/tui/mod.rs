//! The terminal UI shell: a stack of views, a status line, and the background job pump.
//!
//! Each screen lives in its own module and implements `View`. The shell owns the terminal and
//! the event loop; screens never touch either, which is what lets them be tested headless
//! through `testing`.

pub mod accounts;
pub mod browser;
pub mod inbox;
pub mod jobs;
pub mod message;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::aws_profile::{self, CredentialsFile, Profile};
use crate::config::AppConfig;
use crate::s3::{Credentials, S3Client, Store};
use jobs::{Done, Job, JobId, Jobs};

/// What a view asks the shell to do after handling a key or a job result.
pub enum Transition {
    None,
    Push(Box<dyn View>),
    Pop,
    /// Replace the whole stack with this view (e.g. jumping to the inbox after saving it).
    Reset(Box<dyn View>),
    Quit,
}

pub trait View {
    /// Shown in the header bar.
    fn title(&self) -> String;
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx);
    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition;
    /// Every finished job is offered to every view on the stack; ignore ids you did not submit.
    /// Only the top view's transition is applied.
    fn on_done(&mut self, done: &Done, ctx: &mut Ctx) -> Transition {
        let _ = (done, ctx);
        Transition::None
    }
    /// Called once when the view becomes the top of the stack (pushed, or uncovered by a pop).
    fn on_focus(&mut self, ctx: &mut Ctx) {
        let _ = ctx;
    }
    /// Key hints for the footer, e.g. `[("enter", "open"), ("d", "delete")]`.
    fn hints(&self) -> Vec<(&'static str, &'static str)>;
}

/// A connected account.
#[derive(Clone)]
pub struct Session {
    pub profile: Profile,
    pub region: String,
    pub store: Arc<dyn Store>,
}

impl Session {
    /// Region: the caller's hint, else the profile's, else ~/.aws/config, else us-east-1.
    pub fn connect(profile: Profile, region_hint: Option<&str>) -> Self {
        let region = region_hint
            .map(str::to_string)
            .or_else(|| profile.region.clone())
            .or_else(|| aws_profile::region_from_config(&profile.name))
            .unwrap_or_else(|| "us-east-1".to_string());
        let creds = Credentials {
            access_key_id: profile.access_key_id.clone(),
            secret_access_key: profile.secret_access_key.clone(),
            session_token: profile.session_token.clone(),
        };
        let mut client = S3Client::new(creds, &region);
        // Lets the whole app run against MinIO or another S3-compatible endpoint.
        if let Ok(url) = std::env::var("RESES_S3_ENDPOINT") {
            client = client.with_endpoint(&url, true);
        }
        Self {
            profile,
            region,
            store: Arc::new(client),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Info(String),
    Error(String),
}

/// Shared state every view can read and change.
pub struct Ctx {
    pub config: AppConfig,
    pub config_path: PathBuf,
    pub creds_path: PathBuf,
    pub session: Option<Session>,
    pub status: Option<Status>,
    jobs: Jobs,
}

impl Ctx {
    pub fn new(config: AppConfig, config_path: PathBuf, creds_path: PathBuf, jobs: Jobs) -> Self {
        Self {
            config,
            config_path,
            creds_path,
            session: None,
            status: None,
            jobs,
        }
    }

    /// Queue a job against the current session. None when no account is connected.
    pub fn submit(&mut self, job: Job) -> Option<JobId> {
        let store = Arc::clone(&self.session.as_ref()?.store);
        Some(self.jobs.submit(store, job))
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.status = Some(Status::Info(msg.into()));
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.status = Some(Status::Error(msg.into()));
    }

    /// Persist `config`, reporting failure on the status line. Returns whether it saved.
    pub fn save_config(&mut self) -> bool {
        match self.config.save(&self.config_path) {
            Ok(()) => true,
            Err(e) => {
                self.error(format!("could not save settings: {e}"));
                false
            }
        }
    }

    pub fn credentials(&mut self) -> Option<CredentialsFile> {
        match CredentialsFile::load(&self.creds_path) {
            Ok(f) => Some(f),
            Err(e) => {
                self.error(e.to_string());
                None
            }
        }
    }
}

/// The first screen: straight into the saved inbox when there is one, else account selection.
pub fn initial_view(ctx: &mut Ctx) -> Box<dyn View> {
    if let Some(inbox) = ctx.config.inbox.clone() {
        let profile = ctx.credentials().and_then(|f| f.get(&inbox.profile));
        match profile {
            Some(profile) => {
                ctx.session = Some(Session::connect(profile, inbox.region.as_deref()));
                return Box::new(inbox::InboxScreen::new(inbox));
            }
            None => ctx.error(format!(
                "saved inbox uses profile '{}', which is not in {}",
                inbox.profile,
                ctx.creds_path.display()
            )),
        }
    }
    Box::new(accounts::AccountsScreen::new(ctx))
}

/// The view stack plus the job pump, independent of any terminal.
pub struct App {
    pub ctx: Ctx,
    pub stack: Vec<Box<dyn View>>,
    pub quit: bool,
}

impl App {
    pub fn new(mut ctx: Ctx) -> Self {
        let mut first = initial_view(&mut ctx);
        first.on_focus(&mut ctx);
        Self {
            ctx,
            stack: vec![first],
            quit: false,
        }
    }

    pub fn with_view(ctx: Ctx, view: Box<dyn View>) -> Self {
        let mut app = Self {
            ctx,
            stack: vec![view],
            quit: false,
        };
        app.focus_top();
        app
    }

    fn focus_top(&mut self) {
        if let Some(top) = self.stack.last_mut() {
            top.on_focus(&mut self.ctx);
        }
    }

    pub fn apply(&mut self, t: Transition) {
        match t {
            Transition::None => return,
            Transition::Push(v) => self.stack.push(v),
            Transition::Pop => {
                self.stack.pop();
                if self.stack.is_empty() {
                    self.quit = true;
                    return;
                }
            }
            Transition::Reset(v) => {
                self.stack.clear();
                self.stack.push(v);
            }
            Transition::Quit => {
                self.quit = true;
                return;
            }
        }
        self.focus_top();
    }

    pub fn key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        let Some(top) = self.stack.last_mut() else {
            self.quit = true;
            return;
        };
        let t = top.on_key(key, &mut self.ctx);
        self.apply(t);
    }

    /// Hand finished jobs to the views. Returns how many there were.
    pub fn pump(&mut self) -> usize {
        let finished = self.ctx.jobs.poll();
        let n = finished.len();
        for done in finished {
            let last = self.stack.len().saturating_sub(1);
            let mut top_transition = Transition::None;
            for (i, view) in self.stack.iter_mut().enumerate() {
                let t = view.on_done(&done, &mut self.ctx);
                if i == last {
                    top_transition = t;
                }
            }
            self.apply(top_transition);
            if self.quit {
                break;
            }
        }
        n
    }

    pub fn render(&mut self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let Some(top) = self.stack.last_mut() else {
            return;
        };
        let who = match &self.ctx.session {
            Some(s) => format!("  {} ({})", s.profile.name, s.region),
            None => String::new(),
        };
        let bar = Style::default().bg(Color::Blue).fg(Color::White);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" reses ", bar.add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {}{who}", top.title()), bar),
            ]))
            .style(bar),
            header,
        );

        top.render(frame, body, &self.ctx);

        let footer_line = match &self.ctx.status {
            Some(Status::Error(m)) => {
                Line::styled(format!(" {m}"), Style::default().fg(Color::Red))
            }
            Some(Status::Info(m)) => {
                Line::styled(format!(" {m}"), Style::default().fg(Color::Green))
            }
            None => {
                let mut spans = Vec::new();
                for (k, what) in top.hints() {
                    spans.push(Span::styled(
                        format!(" {k} "),
                        Style::default().add_modifier(Modifier::REVERSED),
                    ));
                    spans.push(Span::raw(format!(" {what}  ")));
                }
                Line::from(spans)
            }
        };
        frame.render_widget(Paragraph::new(footer_line), footer);
    }
}

/// Run the interactive UI until the user quits.
pub fn run(config_path: PathBuf, creds_path: PathBuf) -> anyhow::Result<()> {
    let config = AppConfig::load(&config_path)?;
    let ctx = Ctx::new(config, config_path, creds_path, Jobs::pool(8));
    let mut app = App::new(ctx);

    let mut terminal = ratatui::init();
    let result = (|| -> anyhow::Result<()> {
        while !app.quit {
            terminal.draw(|f| app.render(f))?;
            if event::poll(Duration::from_millis(50))?
                && let Event::Key(key) = event::read()?
            {
                // A key press clears the last status message so hints come back.
                app.ctx.status = None;
                app.key(key);
            }
            app.pump();
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

/// Headless helpers for view tests.
#[cfg(test)]
#[allow(dead_code)] // used by the screen modules' tests
pub(crate) mod testing {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEventState;

    /// A Ctx with inline jobs, settings under `dir`, and (optionally) a connected fake store.
    pub fn ctx(dir: &std::path::Path, store: Option<Arc<dyn Store>>) -> Ctx {
        let mut ctx = Ctx::new(
            AppConfig::default(),
            dir.join("config.toml"),
            dir.join("credentials"),
            Jobs::inline(),
        );
        if let Some(store) = store {
            ctx.session = Some(Session {
                profile: Profile {
                    name: "test".into(),
                    access_key_id: "AKIDTEST".into(),
                    secret_access_key: "secret".into(),
                    session_token: None,
                    region: Some("us-east-1".into()),
                },
                region: "us-east-1".into(),
                store,
            });
        }
        ctx
    }

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    pub fn chars(app: &mut App, s: &str) {
        for c in s.chars() {
            app.key(key(KeyCode::Char(c)));
            settle(app);
        }
    }

    /// Pump until no job is left (inline jobs can submit more jobs from on_done).
    pub fn settle(app: &mut App) {
        for _ in 0..1000 {
            if app.pump() == 0 {
                return;
            }
        }
        panic!("jobs never settled");
    }

    /// Render the whole app to plain text, one line per row, trailing spaces trimmed.
    pub fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..height)
            .map(|y| {
                let row: String = (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect();
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
