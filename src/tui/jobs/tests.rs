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

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl Store for Gated {
    fn list_buckets(&self) -> Result<Vec<Bucket>, S3Error> {
        self.enter("buckets".into());
        self.inner.list_buckets()
    }
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
    fn get_range(&self, bucket: &str, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
        self.enter(format!("peek {key}"));
        self.inner.get_range(bucket, key, start, end)
    }
    fn get(&self, bucket: &str, key: &str) -> Result<Vec<u8>, S3Error> {
        self.enter(format!("get {key}"));
        self.inner.get(bucket, key)
    }
    fn delete(&self, bucket: &str, key: &str) -> Result<(), S3Error> {
        self.enter(format!("delete {key}"));
        self.inner.delete(bucket, key)
    }
}

fn email(subject: &str) -> Vec<u8> {
    format!(
        "From: A <a@example.com>\r\nTo: b@example.com\r\nSubject: {subject}\r\n\
         Date: Fri, 25 Sep 2026 09:30:00 +0000\r\nMessage-ID: <{subject}@example.com>\r\n\
         Content-Type: text/plain\r\n\r\nbody of {subject}\r\n"
    )
    .into_bytes()
}

fn peek(key: &str) -> Job {
    Job::Peek {
        bucket: B.into(),
        key: key.into(),
        bytes: 64,
    }
}

fn get(key: &str) -> Job {
    Job::Get {
        bucket: B.into(),
        key: key.into(),
    }
}

fn delete(key: &str) -> Job {
    Job::Delete {
        bucket: B.into(),
        key: key.into(),
    }
}

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
