//! Background S3 work. Views submit a `Job` and get a `Done` back on a later tick, so the UI
//! never blocks on the network. Tests use `Jobs::inline()`, which runs each job on submit.
//!
//! The pool has two queues. Jobs a person is waiting on (opening a message, deleting, listing)
//! always go before header peeks, so a folder of thousands of objects can't hold up Enter.
//! A job can carry a `Stamp` from a view's `Generation`; when the view moves on (a refresh), the
//! workers skip its stale listings and peeks instead of running them. Deletes are never skipped.

use std::collections::{BTreeMap, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
    /// A whole object as bytes. Only the tests use it; the app opens messages with Open.
    #[cfg(test)]
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

    /// What a stale generation may skip: listings, peeks, and an Open whose message screen has
    /// closed. Never a Delete.
    fn is_skippable(&self) -> bool {
        matches!(
            self,
            Job::List { .. } | Job::Peek { .. } | Job::PeekHead { .. } | Job::Open { .. }
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
    /// I start a new generation at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// I move the generation on, which makes every stamp taken so far stale.
    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    /// I stamp a job with the generation as it is right now.
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
    /// I say whether the generation has moved on since this stamp was taken.
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

/// I do the actual work for one job against the store and wrap the result in the matching
/// `Outcome`. `execute` calls me inside `catch_unwind`.
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
        #[cfg(test)]
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

/// I peek the header block of `bucket/key`, growing the range fourfold from `FIRST_PEEK` until the
/// blank line that ends the headers shows up, the object runs out, or I reach `MAX_PEEK`.
/// Then I decide whether it's email and summarize it, so the UI thread never parses.
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

/// Run one queued job, or skip it when its generation has moved on and it's the kind that may
/// be skipped.
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
    /// Set at quit: from then on only deletes run, since nobody is left to read anything else.
    draining: bool,
}

#[derive(Default)]
struct Shared {
    queues: Mutex<Queues>,
    ready: Condvar,
    /// Deletes submitted but not yet run, by id, so quitting can wait for them or name them.
    deletes: Mutex<BTreeMap<JobId, (String, String)>>,
    deleted: Condvar,
}

impl Shared {
    /// The next job worth running, urgent first. None once the pool is closed and empty.
    ///
    /// While draining I drop everything but deletes here, so a queue of peeks can't hold a
    /// delete up at quit.
    fn next(&self) -> Option<Work> {
        let mut q = self.queues.lock().ok()?;
        loop {
            if let Some(w) = q.urgent.pop_front().or_else(|| q.background.pop_front()) {
                if q.draining && !matches!(w.2, Job::Delete { .. }) {
                    continue;
                }
                return Some(w);
            }
            if q.closed {
                return None;
            }
            q = self.ready.wait(q).ok()?;
        }
    }

    /// Stop running anything but deletes, and wake every worker to notice.
    fn drain(&self) {
        if let Ok(mut q) = self.queues.lock() {
            q.draining = true;
            q.closed = true;
        }
        self.ready.notify_all();
    }

    /// A delete ran (or failed): it's no longer outstanding.
    fn delete_ran(&self, id: JobId) {
        if let Ok(mut d) = self.deletes.lock() {
            d.remove(&id);
        }
        self.deleted.notify_all();
    }
}

pub struct Jobs {
    next_id: JobId,
    mode: Mode,
}

enum Mode {
    /// Tests run each job on submit, so a screen's work is done by the next poll.
    #[cfg(test)]
    Inline(VecDeque<Done>),
    Pool {
        shared: Arc<Shared>,
        done: Receiver<Done>,
    },
}

impl Jobs {
    /// Run each job synchronously inside `submit`; results come out of the next `poll`.
    #[cfg(test)]
    pub fn inline() -> Self {
        Self {
            next_id: 1,
            mode: Mode::Inline(VecDeque::new()),
        }
    }

    /// `threads` workers. Several at once so inbox header peeks do not queue behind each other.
    ///
    /// A worker keeps going when nobody reads its results any more (the app has quit and
    /// dropped the receiver), because a delete still in the queue has to reach S3 anyway.
    pub fn pool(threads: usize) -> Self {
        let shared = Arc::new(Shared::default());
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        for _ in 0..threads.max(1) {
            let shared = Arc::clone(&shared);
            let done_tx: Sender<Done> = done_tx.clone();
            thread::spawn(move || {
                while let Some(work) = shared.next() {
                    let done = run_work(work);
                    // Mark the delete done before anyone can see its result.
                    if matches!(done.job, Job::Delete { .. }) {
                        shared.delete_ran(done.id);
                    }
                    let _ = done_tx.send(done);
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

    /// I queue a job with no generation, so nothing can skip it. Only the tests use this.
    #[cfg(test)]
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
            #[cfg(test)]
            Mode::Inline(queue) => queue.push_back(run_work((id, store, job, stamp))),
            Mode::Pool { shared, .. } => {
                // Record a delete before it's queued, so a worker can't finish it first.
                if let Job::Delete { bucket, key } = &job
                    && let Ok(mut d) = shared.deletes.lock()
                {
                    d.insert(id, (bucket.clone(), key.clone()));
                }
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
            #[cfg(test)]
            Mode::Inline(queue) => queue.drain(..).collect(),
            Mode::Pool { done, .. } => done.try_iter().collect(),
        }
    }

    /// At quit: run only the deletes still queued, wait for them up to `limit`, and hand back
    /// (bucket, key) for each one that hadn't finished by then, so the caller can say which.
    pub fn finish(&mut self, limit: Duration) -> Vec<(String, String)> {
        // Only a pool has anything to drain; outside tests a pool is all there is.
        #[allow(irrefutable_let_patterns)]
        let Mode::Pool { shared, .. } = &self.mode else {
            return Vec::new();
        };
        shared.drain();
        let deadline = Instant::now() + limit;
        let Ok(mut left) = shared.deletes.lock() else {
            return Vec::new();
        };
        // Wait until the last one runs or the time is up, whichever comes first.
        while !left.is_empty() {
            let wait = deadline.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                break;
            }
            left = match shared.deleted.wait_timeout(left, wait) {
                Ok((guard, _)) => guard,
                Err(_) => return Vec::new(),
            };
        }
        left.values().cloned().collect()
    }

    /// How many deletes are still queued or running, for the "waiting for" message at quit.
    pub fn deletes_outstanding(&self) -> usize {
        match &self.mode {
            #[cfg(test)]
            Mode::Inline(_) => 0,
            Mode::Pool { shared, .. } => shared.deletes.lock().map_or(0, |d| d.len()),
        }
    }

    /// Wait up to `timeout` for at least one result, then take everything that's finished.
    #[cfg(test)]
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
    /// Closing the pool doesn't abandon deletes: the workers finish the ones still queued and
    /// skip everything else.
    fn drop(&mut self) {
        // Outside tests a pool is all there is, so this always matches there.
        #[allow(irrefutable_let_patterns)]
        if let Mode::Pool { shared, .. } = &self.mode {
            shared.drain();
        }
    }
}

#[cfg(test)]
mod tests;
