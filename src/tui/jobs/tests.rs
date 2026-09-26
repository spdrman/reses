//! Tests for the job pool: which queue a job waits in, what a stale generation skips, panics
//! inside a job, `finish` at shutdown, and how far a header peek reads.
//!
//! I use a store that holds every call at a gate and logs the order calls arrived in, so a
//! test can line jobs up behind a busy worker and then check who went first. Nothing sleeps:
//! the only timer is a ceiling that fails a test when a broken pool would otherwise hang it.

use std::collections::HashSet;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::s3::{Bucket, Listing, MemoryStore, S3Error, Store};

const B: &str = "bk";
/// Only a ceiling for a broken pool; nothing here sleeps or waits on a timer to pass.
const LIMIT: Duration = Duration::from_secs(10);

/// A store whose calls all wait at a gate until the test opens it, and that records the order
/// calls reached it in. `explode` makes a get panic.
struct Gated {
    inner: MemoryStore,
    open: Mutex<bool>,
    opened: Condvar,
    log: Mutex<Vec<String>>,
    logged: Condvar,
}

impl Gated {
    /// I build a shut gate over a store holding one small email per test key.
    fn new() -> Arc<Self> {
        let inner = MemoryStore::new();
        for k in ["busy", "a", "b", "c", "d", "fine"] {
            inner.put(B, k, &email(k));
        }
        Arc::new(Self {
            inner,
            open: Mutex::new(false),
            opened: Condvar::new(),
            log: Mutex::new(Vec::new()),
            logged: Condvar::new(),
        })
    }

    /// I log a call and hold it at the gate until the test opens it. A get of "explode" panics
    /// instead, before it's logged.
    fn enter(&self, what: String) {
        if what == "get explode" {
            panic!("boom on explode");
        }
        self.log.lock().unwrap().push(what);
        self.logged.notify_all();
        let deadline = Instant::now() + LIMIT;
        let mut open = self.open.lock().unwrap();
        while !*open {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "the gate was never opened");
            open = self.opened.wait_timeout(open, left).unwrap().0;
        }
    }

    /// Block until `n` calls have reached the store.
    fn wait_for(&self, n: usize) {
        let deadline = Instant::now() + LIMIT;
        let mut log = self.log.lock().unwrap();
        while log.len() < n {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "only {} calls arrived: {:?}",
                log.len(),
                *log
            );
            log = self.logged.wait_timeout(log, left).unwrap().0;
        }
    }

    /// I open the gate, letting every held call and every later one through.
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }

    /// I return the calls in the order they reached the store.
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl Store for Gated {
    /// I log the call, wait at the gate, then list the buckets.
    fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
        self.enter("buckets".into());
        self.inner.list_buckets()
    }
    /// I log the call, wait at the gate, then list.
    fn list(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        token: Option<&str>,
    ) -> Result<Listing, S3Error> {
        self.enter("list".into());
        self.inner.list(bucket, prefix, delimiter, token)
    }
    /// I log the call as a peek, wait at the gate, then read the range.
    fn get_range(&self, bucket: &str, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
        self.enter(format!("peek {key}"));
        self.inner.get_range(bucket, key, start, end)
    }
    /// I log the call, wait at the gate, then read the object.
    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
        self.enter(format!("get {key}"));
        self.inner.get(bucket, key)
    }
    /// I log the call, wait at the gate, then delete.
    fn delete(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
        self.enter(format!("delete {key}"));
        self.inner.delete(bucket, key)
    }
}

/// I build a small plain text email with `subject` in its headers and body.
fn email(subject: &str) -> Vec<u8> {
    format!(
        "From: A <a@example.com>\r\nTo: b@example.com\r\nSubject: {subject}\r\n\
         Date: Fri, 25 Sep 2026 09:30:00 +0000\r\nMessage-ID: <{subject}@example.com>\r\n\
         Content-Type: text/plain\r\n\r\nbody of {subject}\r\n"
    )
    .into_bytes()
}

/// I build a 64 byte peek of `key`.
fn peek(key: &str) -> Job {
    Job::Peek {
        bucket: B.into(),
        key: key.into(),
        bytes: 64,
    }
}

/// I build a whole-object get of `key`.
fn get(key: &str) -> Job {
    Job::Get {
        bucket: B.into(),
        key: key.into(),
    }
}

/// I build a delete of `key`.
fn delete(key: &str) -> Job {
    Job::Delete {
        bucket: B.into(),
        key: key.into(),
    }
}

/// I build a folder listing of the test bucket's root.
fn list() -> Job {
    Job::List {
        bucket: B.into(),
        prefix: String::new(),
        delimiter: true,
        token: None,
    }
}

/// Collect exactly `n` results, failing rather than hanging if the pool loses one.
fn collect(jobs: &mut Jobs, n: usize) -> Vec<Done> {
    let deadline = Instant::now() + LIMIT;
    let mut out = Vec::new();
    while out.len() < n {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "only {} of {n} jobs came back", out.len());
        out.extend(jobs.wait(left));
    }
    assert_eq!(out.len(), n, "more results than jobs: {out:?}");
    out
}

#[test]
fn jobs_someone_is_waiting_on_run_before_queued_peeks() {
    let store = Gated::new();
    let mut jobs = Jobs::pool(1);
    // The only worker takes this and holds it at the gate.
    jobs.submit(store.clone(), peek("busy"));
    store.wait_for(1);
    jobs.submit(store.clone(), peek("a"));
    jobs.submit(
        store.clone(),
        Job::PeekHead {
            bucket: B.into(),
            key: "b".into(),
        },
    );
    jobs.submit(store.clone(), get("c"));
    jobs.submit(store.clone(), delete("d"));
    jobs.submit(store.clone(), list());
    store.release();
    collect(&mut jobs, 6);
    assert_eq!(
        store.log(),
        ["peek busy", "get c", "delete d", "list", "peek a", "peek b"]
    );
}

#[test]
fn a_stale_generation_skips_listings_and_peeks_but_never_a_delete() {
    let store = Gated::new();
    let mut jobs = Jobs::pool(1);
    let generation = Generation::new();
    jobs.submit(store.clone(), get("busy"));
    store.wait_for(1);
    let queued = [
        list(),
        peek("a"),
        Job::PeekHead {
            bucket: B.into(),
            key: "b".into(),
        },
        delete("c"),
        get("d"),
    ];
    for job in &queued {
        jobs.submit_stamped(store.clone(), job.clone(), Some(generation.stamp()));
    }
    // A refresh: everything above is now stale.
    generation.bump();
    store.release();
    let done = collect(&mut jobs, 6);

    assert_eq!(store.log(), ["get busy", "delete c", "get d"]);
    assert!(!store.inner.contains(B, "c"));
    for d in &done {
        let skipped = matches!(d.result, Ok(Outcome::Skipped));
        let should_skip = matches!(
            d.job,
            Job::List { .. } | Job::Peek { .. } | Job::PeekHead { .. }
        );
        assert_eq!(skipped, should_skip, "{d:?}");
    }
}

#[test]
fn a_current_generation_runs_everything() {
    let store = Gated::new();
    store.release();
    let mut jobs = Jobs::pool(2);
    let generation = Generation::new();
    generation.bump();
    for key in ["a", "b"] {
        jobs.submit_stamped(store.clone(), peek(key), Some(generation.stamp()));
    }
    for d in collect(&mut jobs, 2) {
        assert!(matches!(d.result, Ok(Outcome::Data(_))), "{d:?}");
    }
}

#[test]
fn a_panicking_job_comes_back_as_an_err_and_the_worker_keeps_going() {
    let store = Gated::new();
    store.release();
    let mut jobs = Jobs::pool(1);
    let bad = jobs.submit(store.clone(), get("explode"));
    let done = collect(&mut jobs, 1);
    assert_eq!(done[0].id, bad);
    match &done[0].result {
        Err(S3Error::Transport(m)) => assert!(m.contains("boom on explode"), "{m}"),
        other => panic!("expected an Err, got {other:?}"),
    }
    // The same single worker is still alive to run the next job.
    jobs.submit(store.clone(), get("fine"));
    let done = collect(&mut jobs, 1);
    assert!(matches!(done[0].result, Ok(Outcome::Data(_))), "{done:?}");
}

#[test]
fn every_submitted_job_sends_exactly_one_done() {
    let store = Gated::new();
    store.release();
    let mut jobs = Jobs::pool(4);
    let mut ids = HashSet::new();
    for i in 0..60 {
        let key = ["a", "b", "c", "d"][i % 4];
        let job = if i % 3 == 0 { get(key) } else { peek(key) };
        ids.insert(jobs.submit(store.clone(), job));
    }
    let got: HashSet<JobId> = collect(&mut jobs, 60).iter().map(|d| d.id).collect();
    assert_eq!(got, ids);
    assert!(jobs.poll().is_empty());
}

#[test]
fn peek_head_decides_and_summarizes_on_the_worker() {
    let store = MemoryStore::new();
    store.put(B, "mail", &email("Hello there"));
    store.put(B, "png", b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR");
    let head = |key: &str| {
        execute(
            &store,
            &Job::PeekHead {
                bucket: B.into(),
                key: key.into(),
            },
        )
    };
    match head("mail") {
        Ok(Outcome::Head(h)) => {
            assert!(h.is_email);
            assert_eq!(h.summary.unwrap().subject, "Hello there");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        head("png"),
        Ok(Outcome::Head(Head {
            is_email: false,
            summary: None
        }))
    );
}

#[test]
fn peek_head_reads_past_a_long_header_block() {
    let mut raw = String::new();
    for i in 0..600 {
        raw.push_str(&format!(
            "Received: from relay{i}.example.com id {i:0>80}\r\n"
        ));
    }
    raw.push_str(&String::from_utf8(email("After the relays")).unwrap());
    assert!(raw.len() as u64 > FIRST_PEEK);
    let store = MemoryStore::new();
    store.put(B, "k", raw.as_bytes());
    let out = execute(
        &store,
        &Job::PeekHead {
            bucket: B.into(),
            key: "k".into(),
        },
    );
    match out {
        Ok(Outcome::Head(h)) => assert_eq!(h.summary.unwrap().subject, "After the relays"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn open_decodes_on_the_worker() {
    let store = MemoryStore::new();
    let raw = email("Decoded");
    store.put(B, "k", &raw);
    let out = execute(
        &store,
        &Job::Open {
            bucket: B.into(),
            key: "k".into(),
        },
    );
    match out {
        Ok(Outcome::Message(m)) => {
            assert_eq!(m.raw, raw);
            assert_eq!(m.text, mail::format_message(&raw, false));
            assert_eq!(m.html, mail::format_message(&raw, true));
            assert_eq!(m.summary.subject, "Decoded");
        }
        other => panic!("{other:?}"),
    }
}

/// I build an open of `key`.
fn open(key: &str) -> Job {
    Job::Open {
        bucket: B.into(),
        key: key.into(),
    }
}

/// Wait, bounded, until the store has seen `what`.
fn eventually_logged(store: &Gated, what: &str) -> bool {
    let deadline = Instant::now() + LIMIT;
    let mut log = store.log.lock().unwrap();
    loop {
        if log.iter().any(|l| l == what) {
            return true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        log = store.logged.wait_timeout(log, left).unwrap().0;
    }
}

#[test]
fn a_stale_open_is_skipped_like_a_peek() {
    let store = Gated::new();
    let mut jobs = Jobs::pool(1);
    let generation = Generation::new();
    jobs.submit(store.clone(), get("busy"));
    store.wait_for(1);
    jobs.submit_stamped(store.clone(), open("a"), Some(generation.stamp()));
    jobs.submit_stamped(store.clone(), open("b"), Some(generation.stamp()));
    // The message screen that asked for them closed.
    generation.bump();
    jobs.submit_stamped(store.clone(), open("c"), Some(generation.stamp()));
    store.release();
    let done = collect(&mut jobs, 4);
    assert_eq!(store.log(), ["get busy", "get c"]);
    let skipped: Vec<_> = done
        .iter()
        .filter(|d| matches!(d.result, Ok(Outcome::Skipped)))
        .map(|d| d.job.clone())
        .collect();
    assert_eq!(skipped, [open("a"), open("b")]);
}

#[test]
fn a_delete_queued_when_the_pool_is_dropped_still_runs() {
    let store = Gated::new();
    let mut jobs = Jobs::pool(1);
    jobs.submit(store.clone(), get("busy"));
    store.wait_for(1);
    jobs.submit(store.clone(), delete("c"));
    // Quitting drops the pool, so nothing will ever read its results again.
    drop(jobs);
    store.release();
    assert!(
        eventually_logged(&store, "delete c"),
        "the queued delete never reached S3: {:?}",
        store.log()
    );
    // The log is written as the call arrives; the object goes a moment later.
    let deadline = Instant::now() + LIMIT;
    while store.inner.contains(B, "c") {
        assert!(Instant::now() < deadline, "the delete never finished");
        std::thread::yield_now();
    }
}

#[test]
fn finish_waits_for_queued_deletes_and_skips_everything_else() {
    let store = Gated::new();
    let mut jobs = Jobs::pool(1);
    jobs.submit(store.clone(), get("busy"));
    store.wait_for(1);
    jobs.submit(store.clone(), peek("a"));
    jobs.submit(store.clone(), list());
    jobs.submit(store.clone(), delete("c"));
    // Open the gate only once finish has started draining, so the order is certain.
    let shared = match &jobs.mode {
        Mode::Pool { shared, .. } => Arc::clone(shared),
        Mode::Inline(_) => unreachable!(),
    };
    let opener = {
        let store = store.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + LIMIT;
            while !shared.queues.lock().unwrap().draining {
                assert!(Instant::now() < deadline, "finish never started draining");
                std::thread::yield_now();
            }
            store.release();
        })
    };
    let dropped = jobs.finish(LIMIT);
    opener.join().unwrap();
    assert!(dropped.is_empty(), "{dropped:?}");
    assert!(!store.inner.contains(B, "c"));
    // Nobody is waiting for the rest any more, so it never runs.
    assert_eq!(store.log(), ["get busy", "delete c"]);
}

#[test]
fn finish_names_the_deletes_it_could_not_wait_for() {
    let store = Gated::new();
    let mut jobs = Jobs::pool(1);
    jobs.submit(store.clone(), get("busy"));
    store.wait_for(1);
    jobs.submit(store.clone(), delete("c"));
    jobs.submit(store.clone(), delete("d"));
    // The worker is stuck, so the deadline passes with both deletes still queued.
    let dropped = jobs.finish(Duration::from_millis(200));
    assert_eq!(
        dropped,
        [
            (B.to_string(), "c".to_string()),
            (B.to_string(), "d".to_string())
        ]
    );
    store.release();
}

#[test]
fn finish_on_inline_jobs_has_nothing_to_wait_for() {
    let store = MemoryStore::new();
    store.put(B, "k", b"x");
    let mut jobs = Jobs::inline();
    jobs.submit(Arc::new(store), delete("k"));
    assert!(jobs.finish(Duration::ZERO).is_empty());
}

/// A store that records the byte ranges it was asked for, so a test can see how far a header
/// peek read, not only what it decided.
struct Ranges {
    inner: MemoryStore,
    asked: Mutex<Vec<(u64, u64)>>,
}

impl Ranges {
    /// One object under `k`, and no requests yet.
    fn holding(raw: &[u8]) -> Self {
        let inner = MemoryStore::new();
        inner.put(B, "k", raw);
        Self {
            inner,
            asked: Mutex::new(Vec::new()),
        }
    }

    /// Run a header peek of `k` and return the summary's subject with the ranges it read.
    fn peek_head(&self) -> (Option<String>, Vec<(u64, u64)>) {
        let out = execute(
            self,
            &Job::PeekHead {
                bucket: B.into(),
                key: "k".into(),
            },
        );
        let subject = match out {
            Ok(Outcome::Head(h)) => h.summary.map(|s| s.subject),
            other => panic!("{other:?}"),
        };
        (subject, self.asked.lock().unwrap().clone())
    }
}

impl Store for Ranges {
    /// I pass this straight through to the wrapped store.
    fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
        self.inner.list_buckets()
    }
    /// I pass this straight through to the wrapped store.
    fn list(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        token: Option<&str>,
    ) -> Result<Listing, S3Error> {
        self.inner.list(bucket, prefix, delimiter, token)
    }
    /// I record the range asked for, then read it through the wrapped store.
    fn get_range(&self, bucket: &str, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
        self.asked.lock().unwrap().push((start, end));
        self.inner.get_range(bucket, key, start, end)
    }
    /// I pass this straight through to the wrapped store.
    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
        self.inner.get(bucket, key)
    }
    /// I pass this straight through to the wrapped store.
    fn delete(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
        self.inner.delete(bucket, key)
    }
}

#[test]
fn a_crlf_header_that_ends_in_the_first_peek_stops_there() {
    // SES stores mail with CRLF line endings. When the blank line ending the header block is in
    // the first peek, the peek is done, however big the body behind it is; reading on would
    // fetch up to 32 times as much for every row of the inbox.
    let mut raw = email("Short header, long body");
    raw.extend(std::iter::repeat_n(b'x', 3 * FIRST_PEEK as usize));
    let store = Ranges::holding(&raw);
    let (subject, asked) = store.peek_head();
    assert_eq!(subject.as_deref(), Some("Short header, long body"));
    assert_eq!(
        asked,
        [(0, FIRST_PEEK - 1)],
        "the peek read past the header"
    );
}

#[test]
fn an_object_smaller_than_the_peek_is_read_once() {
    // A short object with no blank line (a header block on its own) came back whole in the
    // first peek, so asking again for more of it can't find anything new.
    let raw = b"From: A <a@example.com>\r\nTo: b@example.com\r\nSubject: Header only\r\n";
    let store = Ranges::holding(raw);
    let (subject, asked) = store.peek_head();
    assert_eq!(subject.as_deref(), Some("Header only"));
    assert_eq!(
        asked,
        [(0, FIRST_PEEK - 1)],
        "the whole object was read more than once"
    );
}
