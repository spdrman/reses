//! Background S3 work. Views submit a `Job` and get a `Done` back on a later tick, so the UI
//! never blocks on the network. Tests use `Jobs::inline()`, which runs each job on submit.
//!
//! The pool has two queues. Jobs a person is waiting on (opening a message, deleting, listing)
//! always go before header peeks, so a folder of thousands of objects can't hold up Enter.
//! A job can carry a `Stamp` from a view's `Generation`; when the view moves on (a refresh), the
//! workers skip its stale listings and peeks instead of running them. Deletes are never skipped.

use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crate::mail::{self, Summary};
use crate::s3::{Bucket, Listing, S3Error, Store};

pub type JobId = u64;

/// Where a header peek starts. Most header blocks fit.
pub const FIRST_PEEK: u64 = 32 * 1024;
/// A header peek grows up to this and then summarizes whatever arrived.
pub const MAX_PEEK: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    ListBuckets,
    /// One page of keys. `delimiter: true` groups by "/".
    List {
        bucket: String,
        prefix: String,
        delimiter: bool,
        token: Option<String>,
    },
    /// The first `bytes` bytes of an object, for email detection and inbox headers.
    Peek {
        bucket: String,
        key: String,
        bytes: u64,
    },
    Get {
        bucket: String,
        key: String,
    },
    Delete {
        bucket: String,
        key: String,
    },
    /// Peek the header block (growing past `FIRST_PEEK` when it hasn't ended), decide whether
    /// it's email and summarize it, all on the worker. Answers with `Outcome::Head`.
    PeekHead {
        bucket: String,
        key: String,
    },
    /// Fetch a whole message and decode it on the worker. Answers with `Outcome::Message`.
    Open {
        bucket: String,
        key: String,
    },
}

impl Job {
    /// Header peeks wait behind everything a person is waiting on.
    fn is_background(&self) -> bool {
        matches!(self, Job::Peek { .. } | Job::PeekHead { .. })
    }

    /// What a stale generation may skip. Never a Delete, Get or Open.
    fn is_skippable(&self) -> bool {
        matches!(
            self,
            Job::List { .. } | Job::Peek { .. } | Job::PeekHead { .. }
        )
    }
}

/// A header peek, decided and summarized on the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub is_email: bool,
    /// Set when `is_email`.
    pub summary: Option<Summary>,
}

/// A whole message, decoded on the worker, both ways round so the HTML toggle is instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub raw: Vec<u8>,
    pub text: String,
    pub html: String,
    pub summary: Summary,
}

impl Decoded {
    /// Put a decoded message together. I keep construction in one place so the fields can grow
    /// without every caller having to change.
    pub fn new(raw: Vec<u8>, text: String, html: String, summary: Summary) -> Self {
        Self {
            raw,
            text,
            html,
            summary,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Buckets(Vec<Bucket>),
    Listing(Listing),
    /// Peek and Get both answer with the bytes.
    Data(Vec<u8>),
    Deleted,
    Head(Head),
    /// Shared rather than boxed, so every view the result is offered to can keep it without
    /// copying a message that may be tens of megabytes.
    Message(Arc<Decoded>),
    /// The job's generation had moved on before a worker reached it, so it never ran.
    Skipped,
}

#[derive(Debug, Clone)]
pub struct Done {
    pub id: JobId,
    pub job: Job,
    pub result: Result<Outcome, S3Error>,
}

/// A counter a view bumps when everything it queued so far is no longer wanted.
#[derive(Debug, Clone, Default)]
pub struct Generation(Arc<AtomicU64>);

impl Generation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    pub fn stamp(&self) -> Stamp {
        Stamp {
            generation: Arc::clone(&self.0),
            at: self.0.load(Ordering::SeqCst),
        }
    }
}

/// The generation a job was queued in.
#[derive(Debug, Clone)]
pub struct Stamp {
    generation: Arc<AtomicU64>,
    at: u64,
}

impl Stamp {
    pub fn is_current(&self) -> bool {
        self.generation.load(Ordering::SeqCst) == self.at
    }
}

/// Run one job. A panic inside it (a decoder bug on a strange message, say) comes back as an
/// Err instead of taking the worker thread down.
pub fn execute(store: &dyn Store, job: &Job) -> Result<Outcome, S3Error> {
    panic::catch_unwind(AssertUnwindSafe(|| run_job(store, job))).unwrap_or_else(|payload| {
        let why = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown cause".into());
        Err(S3Error::Transport(format!("the job crashed: {why}")))
    })
}

fn run_job(store: &dyn Store, job: &Job) -> Result<Outcome, S3Error> {
    match job {
        Job::ListBuckets => store.list_buckets().map(Outcome::Buckets),
        Job::List {
            bucket,
            prefix,
            delimiter,
            token,
        } => store
            .list(bucket, prefix, delimiter.then_some("/"), token.as_deref())
            .map(Outcome::Listing),
        Job::Peek { bucket, key, bytes } => store
            .get_range(bucket, key, 0, bytes.saturating_sub(1))
            .map(Outcome::Data),
        Job::Get { bucket, key } => store.get(bucket, key).map(Outcome::Data),
        Job::Delete { bucket, key } => store.delete(bucket, key).map(|()| Outcome::Deleted),
        Job::PeekHead { bucket, key } => peek_head(store, bucket, key).map(Outcome::Head),
        Job::Open { bucket, key } => {
            let raw = store.get(bucket, key)?;
            let text = mail::format_message(&raw, false);
            let html = mail::format_message(&raw, true);
            let summary = mail::summarize(&raw);
            Ok(Outcome::Message(Arc::new(Decoded::new(
                raw, text, html, summary,
            ))))
        }
    }
}

fn peek_head(store: &dyn Store, bucket: &str, key: &str) -> Result<Head, S3Error> {
    let mut bytes = FIRST_PEEK;
    loop {
        let data = store.get_range(bucket, key, 0, bytes - 1)?;
        if !mail::looks_like_email(&data) {
            return Ok(Head {
                is_email: false,
                summary: None,
            });
        }
        let whole_object = (data.len() as u64) < bytes;
        if header_ended(&data) || whole_object || bytes >= MAX_PEEK {
            return Ok(Head {
                is_email: true,
                summary: Some(mail::summarize(&data)),
            });
        }
        bytes = (bytes * 4).min(MAX_PEEK);
    }
}

/// True once the blank line that ends the header block is in the bytes we have.
fn header_ended(data: &[u8]) -> bool {
    data.windows(2).any(|w| w == b"\n\n") || data.windows(4).any(|w| w == b"\r\n\r\n")
}

type Work = (JobId, Arc<dyn Store>, Job, Option<Stamp>);

fn run_work((id, store, job, stamp): Work) -> Done {
    let stale = stamp.is_some_and(|s| !s.is_current());
    let result = if stale && job.is_skippable() {
        Ok(Outcome::Skipped)
    } else {
        execute(store.as_ref(), &job)
    };
    Done { id, job, result }
}

#[derive(Default)]
struct Queues {
    urgent: VecDeque<Work>,
    background: VecDeque<Work>,
    closed: bool,
}

#[derive(Default)]
struct Shared {
    queues: Mutex<Queues>,
    ready: Condvar,
}

impl Shared {
    /// The next job, urgent first. None once the pool is closed.
    fn next(&self) -> Option<Work> {
        let mut q = self.queues.lock().ok()?;
        loop {
            if let Some(w) = q.urgent.pop_front().or_else(|| q.background.pop_front()) {
                return Some(w);
            }
            if q.closed {
                return None;
            }
            q = self.ready.wait(q).ok()?;
        }
    }
}

pub struct Jobs {
    next_id: JobId,
    mode: Mode,
}

enum Mode {
    Inline(VecDeque<Done>),
    Pool {
        shared: Arc<Shared>,
        done: Receiver<Done>,
    },
}

impl Jobs {
    /// Run each job synchronously inside `submit`; results come out of the next `poll`.
    pub fn inline() -> Self {
        Self {
            next_id: 1,
            mode: Mode::Inline(VecDeque::new()),
        }
    }

    /// `threads` workers. Several at once so inbox header peeks do not queue behind each other.
    pub fn pool(threads: usize) -> Self {
        let shared = Arc::new(Shared::default());
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        for _ in 0..threads.max(1) {
            let shared = Arc::clone(&shared);
            let done_tx: Sender<Done> = done_tx.clone();
            thread::spawn(move || {
                while let Some(work) = shared.next() {
                    if done_tx.send(run_work(work)).is_err() {
                        break;
                    }
                }
            });
        }
        Self {
            next_id: 1,
            mode: Mode::Pool {
                shared,
                done: done_rx,
            },
        }
    }

    pub fn submit(&mut self, store: Arc<dyn Store>, job: Job) -> JobId {
        self.submit_stamped(store, job, None)
    }

    /// Like `submit`, tagged with the generation it belongs to.
    pub fn submit_stamped(
        &mut self,
        store: Arc<dyn Store>,
        job: Job,
        stamp: Option<Stamp>,
    ) -> JobId {
        let id = self.next_id;
        self.next_id += 1;
        match &mut self.mode {
            Mode::Inline(queue) => queue.push_back(run_work((id, store, job, stamp))),
            Mode::Pool { shared, .. } => {
                if let Ok(mut q) = shared.queues.lock() {
                    if job.is_background() {
                        q.background.push_back((id, store, job, stamp));
                    } else {
                        q.urgent.push_back((id, store, job, stamp));
                    }
                }
                shared.ready.notify_one();
            }
        }
        id
    }

    /// Everything finished since the last poll, without blocking.
    pub fn poll(&mut self) -> Vec<Done> {
        match &mut self.mode {
            Mode::Inline(queue) => queue.drain(..).collect(),
            Mode::Pool { done, .. } => done.try_iter().collect(),
        }
    }

    /// Stub for the red tests.
    pub fn finish(&mut self, limit: Duration) -> Vec<(String, String)> {
        let _ = limit;
        Vec::new()
    }

    /// Wait up to `timeout` for at least one result, then take everything that's finished.
    pub fn wait(&mut self, timeout: Duration) -> Vec<Done> {
        match &mut self.mode {
            Mode::Inline(queue) => queue.drain(..).collect(),
            Mode::Pool { done, .. } => {
                let mut out: Vec<Done> = done.recv_timeout(timeout).into_iter().collect();
                out.extend(done.try_iter());
                out
            }
        }
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        if let Mode::Pool { shared, .. } = &self.mode {
            if let Ok(mut q) = shared.queues.lock() {
                q.closed = true;
            }
            shared.ready.notify_all();
        }
    }
}

#[cfg(test)]
mod tests;
