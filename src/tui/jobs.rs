//! Background S3 work. Views submit a `Job` and get a `Done` back on a later tick, so the UI
//! never blocks on the network. Tests use `Jobs::inline()`, which runs each job on submit.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::s3::{Bucket, Listing, S3Error, Store};

pub type JobId = u64;

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Buckets(Vec<Bucket>),
    Listing(Listing),
    /// Peek and Get both answer with the bytes.
    Data(Vec<u8>),
    Deleted,
}

#[derive(Debug, Clone)]
pub struct Done {
    pub id: JobId,
    pub job: Job,
    pub result: Result<Outcome, S3Error>,
}

pub fn execute(store: &dyn Store, job: &Job) -> Result<Outcome, S3Error> {
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
    }
}

type Work = (JobId, Arc<dyn Store>, Job);

pub struct Jobs {
    next_id: JobId,
    mode: Mode,
}

enum Mode {
    Inline(VecDeque<Done>),
    Pool {
        work: Sender<Work>,
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
        let (work_tx, work_rx) = mpsc::channel::<Work>();
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        let work_rx = Arc::new(Mutex::new(work_rx));
        for _ in 0..threads.max(1) {
            let work_rx = Arc::clone(&work_rx);
            let done_tx = done_tx.clone();
            thread::spawn(move || {
                loop {
                    // Hold the lock only while taking the next item.
                    let next = work_rx.lock().map(|rx| rx.recv());
                    let Ok(Ok((id, store, job))) = next else {
                        break;
                    };
                    let result = execute(store.as_ref(), &job);
                    if done_tx.send(Done { id, job, result }).is_err() {
                        break;
                    }
                }
            });
        }
        Self {
            next_id: 1,
            mode: Mode::Pool {
                work: work_tx,
                done: done_rx,
            },
        }
    }

    pub fn submit(&mut self, store: Arc<dyn Store>, job: Job) -> JobId {
        let id = self.next_id;
        self.next_id += 1;
        match &mut self.mode {
            Mode::Inline(queue) => {
                let result = execute(store.as_ref(), &job);
                queue.push_back(Done { id, job, result });
            }
            Mode::Pool { work, .. } => {
                // The workers only stop when this sender drops, so send cannot fail here.
                let _ = work.send((id, store, job));
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
}
