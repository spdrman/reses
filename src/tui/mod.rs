//! The terminal UI shell: a stack of views, a status line, and the background job pump.
//!
//! Each screen lives in its own module and implements `View`. The shell owns the terminal and
//! the event loop; screens never touch either, which is what lets them be tested headless
//! through `testing`.

pub(crate) mod accounts;
pub(crate) mod brand;
pub(crate) mod browser;
pub(crate) mod inbox;
pub(crate) mod jobs;
pub(crate) mod message;
pub mod saved;
pub mod text;

use std::panic::PanicHookInfo;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::ThreadId;
use std::time::Duration;

use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::aws_profile::{CredentialsFile, Profile};
use crate::config::AppConfig;
use crate::s3::{S3Client, Store};
use jobs::{Done, Generation, Job, JobId, Jobs};
use time::UtcOffset;

/// What a view asks the shell to do after handling a key or a job result.
pub(crate) enum Transition {
    None,
    Push(Box<dyn View>),
    Pop,
    /// Replace the whole stack with this view (e.g. jumping to the inbox after saving it).
    Reset(Box<dyn View>),
}

pub(crate) trait View {
    /// Shown in the header bar.
    fn title(&self) -> String;
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx);
    fn on_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Transition;
    /// Text pasted into the terminal, whole. It never arrives as keys (bracketed paste is on),
    /// so a paste can't press d and then y; a view with a text input takes it, the rest ignore it.
    fn on_paste(&mut self, text: &str, ctx: &mut Ctx) -> Transition {
        let _ = (text, ctx);
        Transition::None
    }
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
    /// Called on the top view on every pump, after any finished jobs. For work that depends
    /// on what the last render showed, such as peeking the rows now on screen.
    fn on_tick(&mut self, ctx: &mut Ctx) {
        let _ = ctx;
    }
    /// Key hints for the footer, e.g. `[("enter", "open"), ("d", "delete")]`.
    fn hints(&self) -> Vec<(&'static str, &'static str)>;
    /// The account this view works in, when it holds its own. The header bar shows it in
    /// place of `ctx.session`.
    fn session(&self) -> Option<&Session> {
        None
    }
}

/// A connected account.
#[derive(Clone)]
pub(crate) struct Session {
    pub profile: Profile,
    pub region: String,
    pub store: Arc<dyn Store>,
}

impl Session {
    /// I connect by profile name and let aws-config resolve the credentials from the AWS files
    /// itself, so static keys, session tokens, SSO, assume-role and credential_process all work.
    /// A `Profile` read from the credentials file still works here (only its name is used for
    /// credentials), so existing callers don't change. The region is the caller's hint, else the
    /// profile's region from the AWS files, else us-east-1.
    pub fn connect(profile: impl Into<Profile>, region_hint: Option<&str>) -> Self {
        let profile = profile.into();
        let mut client = S3Client::from_profile(&profile.name, region_hint);
        // RESES_S3_ENDPOINT lets the whole app run against MinIO or another S3-compatible
        // endpoint, with the bucket in the path.
        if let Ok(url) = std::env::var("RESES_S3_ENDPOINT") {
            client = client.with_endpoint(&url, true);
        }
        let region = client.region().to_string();
        Self {
            profile,
            region,
            store: Arc::new(client),
        }
    }
}

/// A bare profile name as a `Profile`, so `Session::connect("work", None)` reads naturally. The
/// keys stay empty: the SDK reads them from the AWS files when it needs them.
impl From<&str> for Profile {
    fn from(name: &str) -> Self {
        Profile {
            name: name.to_string(),
            ..Profile::default()
        }
    }
}

/// The owned-string form of the conversion above.
impl From<String> for Profile {
    fn from(name: String) -> Self {
        Profile {
            name,
            ..Profile::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    Info(String),
    Error(String),
}

/// Shared state every view can read and change.
pub(crate) struct Ctx {
    pub config: AppConfig,
    pub config_path: PathBuf,
    pub creds_path: PathBuf,
    pub session: Option<Session>,
    pub status: Option<Status>,
    /// The local UTC offset, read once at startup before any thread exists (on Unix the
    /// lookup fails once there are other threads). One offset for the whole run means a
    /// message from the other side of a DST change shows an hour off; that's accepted.
    pub local_offset: UtcOffset,
    /// How the header draws the logo: the real image when the terminal can show one, the
    /// styled-text wordmark otherwise. Detected once at startup.
    pub brand: brand::Brand,
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
            local_offset: UtcOffset::UTC,
            brand: brand::Brand::text(),
            jobs,
        }
    }

    pub fn with_local_offset(mut self, offset: UtcOffset) -> Self {
        self.local_offset = offset;
        self
    }

    pub fn with_brand(mut self, brand: brand::Brand) -> Self {
        self.brand = brand;
        self
    }

    /// Queue a job against the current session. None when no account is connected.
    #[cfg(test)]
    pub fn submit(&mut self, job: Job) -> Option<JobId> {
        let store = Arc::clone(&self.session.as_ref()?.store);
        Some(self.jobs.submit(store, job))
    }

    /// Queue a job against a session the caller holds, rather than whichever account is
    /// current, so a view keeps talking to the account it was opened with. With a
    /// generation, a later bump lets the workers skip it if it's a listing or a peek.
    pub fn submit_to(
        &mut self,
        session: &Session,
        job: Job,
        generation: Option<&Generation>,
    ) -> JobId {
        self.jobs.submit_stamped(
            Arc::clone(&session.store),
            job,
            generation.map(Generation::stamp),
        )
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
pub(crate) fn initial_view(ctx: &mut Ctx) -> Box<dyn View> {
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
pub(crate) struct App {
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

    #[cfg(test)]
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

    /// Hand a paste to the top view.
    pub fn paste(&mut self, text: &str) {
        let Some(top) = self.stack.last_mut() else {
            return;
        };
        let t = top.on_paste(text, &mut self.ctx);
        self.apply(t);
    }

    /// Hand finished jobs to the views. Returns how many there were.
    pub fn pump(&mut self) -> usize {
        // Tick first, so whatever the top view queues is collected by this same pump.
        if let Some(top) = self.stack.last_mut() {
            top.on_tick(&mut self.ctx);
        }
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
        let full = frame.area();
        // The image logo when there's one and room for it; otherwise the one-row text bar.
        let image = self.ctx.brand.image_for(full.width, full.height);
        let header_rows = if image.is_some() {
            brand::IMAGE_ROWS
        } else {
            1
        };
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(header_rows),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(full);

        let Some(top) = self.stack.last_mut() else {
            return;
        };
        let account = top.session().or(self.ctx.session.as_ref()).map(|s| {
            format!(
                "{} ({})",
                text::escape(&s.profile.name),
                text::escape(&s.region)
            )
        });
        // Titles carry keys, buckets and prefixes straight from S3.
        let title = text::escape(&top.title()).into_owned();
        let bar = brand::bar_style();
        match image {
            Some((logo, cols)) => {
                let [logo_area, _gap, rest] = Layout::horizontal([
                    Constraint::Length(cols),
                    Constraint::Length(1),
                    Constraint::Min(0),
                ])
                .areas(header);
                frame.render_stateful_widget(
                    ratatui_image::StatefulImage::default(),
                    logo_area,
                    logo,
                );
                let lines = vec![
                    Line::from(format!(" {title}")),
                    Line::from(account.map(|a| format!(" {a}")).unwrap_or_default()),
                ];
                frame.render_widget(Paragraph::new(lines).style(bar), rest);
            }
            None => {
                let mut spans = brand::wordmark(bar);
                let who = account.map(|a| format!("  {a}")).unwrap_or_default();
                spans.push(Span::styled(format!("  {title}{who}"), bar));
                frame.render_widget(Paragraph::new(Line::from(spans)).style(bar), header);
            }
        }

        top.render(frame, body, &self.ctx);

        // Errors say so in words as well as in red, so NO_COLOR or a theme where red is
        // faint still tells them from news. Info is the terminal's own colour, which reads on
        // every theme, where green did not on a light one.
        let footer_line = match &self.ctx.status {
            Some(Status::Error(m)) => Line::styled(
                format!(" error: {}", text::escape(m)),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Some(Status::Info(m)) => Line::raw(format!(" {}", text::escape(m))),
            None => hints_line(&top.hints(), footer.width as usize),
        };
        frame.render_widget(Paragraph::new(footer_line), footer);
    }
}

/// The key hints that fit in `cols` columns. Views list hints most important first, so the
/// ones that don't fit come off the end, except `q`, which always stays: without it an 80
/// column terminal lost the only hint for getting out.
fn hints_line(hints: &[(&'static str, &'static str)], cols: usize) -> Line<'static> {
    let cost = |(k, what): &(&str, &str)| text::width(k) + text::width(what) + 5;
    let quit = hints.iter().find(|(k, _)| *k == "q").copied();
    let mut budget = cols.saturating_sub(quit.as_ref().map_or(0, cost));
    let mut kept: Vec<(&'static str, &'static str)> = Vec::new();
    // Take hints in order while they fit, then put q back at the end.
    for h in hints.iter().filter(|(k, _)| *k != "q") {
        if cost(h) > budget {
            break;
        }
        budget -= cost(h);
        kept.push(*h);
    }
    kept.extend(quit);
    let mut spans = Vec::new();
    for (k, what) in kept {
        spans.push(Span::styled(
            format!(" {k} "),
            Style::default().add_modifier(Modifier::REVERSED),
        ));
        spans.push(Span::raw(format!(" {what}  ")));
    }
    Line::from(spans)
}

/// How long quitting waits for deletes still on their way to S3.
const QUIT_DRAIN: Duration = Duration::from_secs(5);

/// Put the terminal into the app's mode: raw, the alternate screen, and bracketed paste, so a
/// paste arrives as one event and not as keys.
fn enter_screen() -> std::io::Result<()> {
    ratatui::crossterm::terminal::enable_raw_mode()?;
    ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::terminal::EnterAlternateScreen,
        ratatui::crossterm::event::EnableBracketedPaste
    )
}

/// Undo `enter_screen`, leaving the terminal as the shell expects it.
fn leave_screen() {
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableBracketedPaste
    );
    ratatui::restore();
}

/// The line printed after the terminal is back, for each delete that hadn't finished when the
/// wait at quit ran out.
fn dropped_delete_line(bucket: &str, key: &str) -> String {
    format!(
        "reses: quit before this delete finished, so it may not have happened: s3://{}/{}",
        text::escape(bucket),
        text::escape(key)
    )
}

/// Signals that should end the app through its own exit, restoring the terminal on the way:
/// a kill, a closed terminal, or an interrupt sent from outside (ctrl-c itself is a key here).
#[cfg(unix)]
fn quit_on_signals() -> std::io::Result<Arc<std::sync::atomic::AtomicBool>> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    for sig in [SIGTERM, SIGINT, SIGHUP] {
        signal_hook::flag::register(sig, Arc::clone(&flag))?;
    }
    Ok(flag)
}

/// Ctrl-z: hand the terminal back, stop the way any program does, and take the screen again
/// when the shell resumes us.
#[cfg(unix)]
fn suspend<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
) -> std::io::Result<()> {
    leave_screen();
    // The default action for SIGTSTP stops the process; this returns after SIGCONT.
    signal_hook::low_level::emulate_default_handler(signal_hook::consts::SIGTSTP)?;
    enter_screen()?;
    terminal.clear()
}

/// Run the interactive UI until the user quits.
/// `local_offset` has to be read before this, while the process still has one thread.
pub fn run(
    config_path: PathBuf,
    creds_path: PathBuf,
    local_offset: UtcOffset,
) -> anyhow::Result<()> {
    let config = AppConfig::load(&config_path)?;
    #[cfg(unix)]
    let signalled = quit_on_signals()?;

    let mut terminal = ratatui::init();
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableBracketedPaste
    );
    // After entering the alternate screen, as the picker asks, and before the job pool:
    // like the local offset, it's read while nothing else is running.
    let brand = brand::Brand::detect();
    let ctx = Ctx::new(config, config_path, creds_path, Jobs::pool(8))
        .with_local_offset(local_offset)
        .with_brand(brand);
    let mut app = App::new(ctx);
    // ratatui's hook restores the terminal on any thread's panic, which would drop the screen
    // under a still-running app when a worker panics. Workers catch their own panics, so only
    // a panic on this thread gets the restore (and the report).
    let restore_and_report = std::panic::take_hook();
    std::panic::set_hook(only_on_thread(
        std::thread::current().id(),
        restore_and_report,
    ));
    let result = (|| -> anyhow::Result<()> {
        while !app.quit {
            #[cfg(unix)]
            if signalled.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            terminal.draw(|f| app.render(f))?;
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    #[cfg(unix)]
                    Event::Key(key)
                        if key.kind == KeyEventKind::Press
                            && key.code == KeyCode::Char('z')
                            && key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        suspend(&mut terminal)?;
                    }
                    Event::Key(key) => {
                        // A key press clears the last status message so hints come back.
                        app.ctx.status = None;
                        app.key(key);
                    }
                    Event::Paste(text) => {
                        app.ctx.status = None;
                        app.paste(&text);
                    }
                    _ => {}
                }
            }
            app.pump();
        }
        // Deletes still queued get a bounded wait, with the screen saying what it waits for.
        let waiting = app.ctx.jobs.deletes_outstanding();
        if waiting > 0 {
            let noun = if waiting == 1 { "delete" } else { "deletes" };
            app.ctx.info(format!(
                "Waiting for {waiting} {noun} to reach S3 before quitting..."
            ));
            terminal.draw(|f| app.render(f))?;
        }
        Ok(())
    })();
    let dropped = app.ctx.jobs.finish(QUIT_DRAIN);
    leave_screen();
    for (bucket, key) in &dropped {
        eprintln!("{}", dropped_delete_line(bucket, key));
    }
    result
}

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

/// A panic hook that runs `hook` for panics on thread `main` and ignores all others.
fn only_on_thread(main: ThreadId, hook: PanicHook) -> PanicHook {
    Box::new(move |info| {
        if std::thread::current().id() == main {
            hook(info);
        }
    })
}

#[cfg(test)]
mod review_tests;
#[cfg(test)]
mod session_tests;

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

    /// Render the whole app and hand back the cells, styles included.
    pub fn buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        term.backend().buffer().clone()
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

#[cfg(test)]
mod shell_tests {
    #[test]
    fn a_delete_that_missed_the_quit_is_named_escaped() {
        let line = super::dropped_delete_line("bk", "mail/a\u{1b}]52;c;eA==\u{7}");
        assert_eq!(
            line,
            "reses: quit before this delete finished, so it may not have happened: \
             s3://bk/mail/a\\x1b]52;c;eA==\\x07"
        );
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::s3::{Bucket, Listing, MemoryStore, S3Error};

    /// Panics on every get, the way a decoder bug on a strange message would.
    struct Exploding;

    impl Store for Exploding {
        fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
            Ok(Vec::new())
        }
        fn list(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<Listing, S3Error> {
            Ok(Listing::default())
        }
        fn get_range(&self, _: &str, _: &str, _: u64, _: u64) -> Result<Vec<u8>, S3Error> {
            Ok(Vec::new())
        }
        fn get(&self, _: &str, _: &str) -> Result<Vec<u8>, S3Error> {
            panic!("worker blew up")
        }
        fn delete(&self, _: &str, _: &str) -> Result<(), S3Error> {
            Ok(())
        }
    }

    #[test]
    fn only_a_panic_on_the_ui_thread_restores_the_terminal() {
        let restores = Arc::new(AtomicUsize::new(0));
        // A stand-in for the UI thread, parked until the hook is in place.
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let ui = std::thread::spawn(move || {
            go_rx.recv().unwrap();
            panic!("ui thread panic");
        });
        let counted = Arc::clone(&restores);
        let previous = std::panic::take_hook();
        std::panic::set_hook(only_on_thread(
            ui.thread().id(),
            Box::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
            }),
        ));

        // A worker panic: the job comes back as an Err and the terminal is left alone.
        let mut jobs = Jobs::pool(1);
        jobs.submit(
            Arc::new(Exploding),
            Job::Get {
                bucket: "b".into(),
                key: "k".into(),
            },
        );
        let done = jobs.wait(Duration::from_secs(10));
        let worker_restores = restores.load(Ordering::SeqCst);

        // A panic on the UI thread still restores.
        go_tx.send(()).unwrap();
        let ui_result = ui.join();
        let ui_restores = restores.load(Ordering::SeqCst);
        std::panic::set_hook(previous);

        assert_eq!(done.len(), 1);
        assert!(done[0].result.is_err(), "{:?}", done[0].result);
        assert_eq!(worker_restores, 0);
        assert!(ui_result.is_err());
        assert_eq!(ui_restores, 1);
    }

    #[test]
    fn submit_to_uses_the_session_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let current = Arc::new(MemoryStore::new());
        let mut ctx = testing::ctx(dir.path(), Some(current.clone()));
        let mine = Arc::new(MemoryStore::new());
        mine.put("b", "k", b"mine");
        let mut session = ctx.session.clone().unwrap();
        session.store = mine.clone();

        ctx.submit_to(
            &session,
            Job::Delete {
                bucket: "b".into(),
                key: "k".into(),
            },
            None,
        );
        let done = ctx.jobs.poll();
        assert_eq!(done.len(), 1);
        assert!(done[0].result.is_ok(), "{:?}", done[0].result);
        assert!(!mine.contains("b", "k"));
    }

    #[test]
    fn the_local_offset_defaults_to_utc_and_can_be_set() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), None);
        assert_eq!(ctx.local_offset, UtcOffset::UTC);
        let east = UtcOffset::from_hms(2, 0, 0).unwrap();
        assert_eq!(ctx.with_local_offset(east).local_offset, east);
    }

    #[test]
    fn the_header_names_the_top_views_own_account() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = testing::ctx(dir.path(), Some(Arc::new(MemoryStore::new())));
        let mut other = ctx.session.clone().unwrap();
        other.profile.name = "work".into();
        other.region = "eu-west-2".into();

        let inbox = inbox::InboxScreen::new(crate::config::Inbox {
            profile: "test".into(),
            bucket: "b".into(),
            prefix: String::new(),
            region: None,
        });
        let mut app = App::with_view(ctx, Box::new(inbox));
        testing::settle(&mut app);
        let header = |app: &mut App| {
            testing::screen(app, 100, 5)
                .lines()
                .next()
                .unwrap()
                .to_string()
        };

        // Another account connects: the inbox still says whose it is.
        let personal = app.ctx.session.replace(other.clone()).unwrap();
        assert!(
            header(&mut app).contains("test (us-east-1)"),
            "{}",
            header(&mut app)
        );

        // A message opened in the other account, on top of the inbox, names that one.
        let message = message::MessageScreen::new("b".into(), "k".into()).with_session(other);
        app.apply(Transition::Push(Box::new(message)));
        testing::settle(&mut app);
        assert!(
            header(&mut app).contains("work (eu-west-2)"),
            "{}",
            header(&mut app)
        );

        // Back on the inbox, its own account again.
        app.apply(Transition::Pop);
        assert!(
            header(&mut app).contains("test (us-east-1)"),
            "{}",
            header(&mut app)
        );

        // A view with no session of its own shows whatever is connected.
        app.ctx.session = Some(personal);
        let accounts = accounts::AccountsScreen::new(&mut app.ctx);
        app.apply(Transition::Push(Box::new(accounts)));
        app.ctx.session.as_mut().unwrap().profile.name = "current".into();
        assert!(
            header(&mut app).contains("current (us-east-1)"),
            "{}",
            header(&mut app)
        );
    }
}
