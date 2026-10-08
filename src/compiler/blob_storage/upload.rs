//! Background publication to the remote cache.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::Context as _;
use parking_lot::Mutex;

use super::exchange;
use super::format::CacheEntry;
use super::remote::RemoteCache;
use crate::CargoResult;

/// Upload workers per invocation. Each worker's transfers are already batched
/// and concurrent, so a few workers keep the connection busy.
const WORKERS: usize = 4;

/// Remote cache state shared with upload workers. The first failure disables
/// further remote operations for the invocation.
pub(super) struct Remote {
    pub cache: RemoteCache,
    error: Mutex<Option<String>>,
}

impl Remote {
    pub fn new(cache: RemoteCache) -> Self {
        Self {
            cache,
            error: Mutex::default(),
        }
    }

    pub fn usable(&self) -> bool {
        self.error.lock().is_none()
    }

    pub fn failed(&self, error: anyhow::Error) {
        self.error
            .lock()
            .get_or_insert_with(|| format!("{error:#}"));
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().clone()
    }
}

struct Upload {
    unit_hash: String,
    entry: CacheEntry,
}

/// Uploads run on background threads so compilation does not wait on the
/// network. `finish` drains the queue while the build still holds its storage
/// lock, so collection cannot remove blobs that are being uploaded.
pub(super) struct Uploader {
    sender: mpsc::Sender<Upload>,
    workers: Vec<JoinHandle<()>>,
    pending: Arc<AtomicUsize>,
}

impl Uploader {
    pub fn start(remote: Arc<Remote>, root: PathBuf) -> CargoResult<Self> {
        let (sender, receiver) = mpsc::channel::<Upload>();
        let receiver = Arc::new(Mutex::new(receiver));
        let pending = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::with_capacity(WORKERS);
        for _ in 0..WORKERS {
            let receiver = Arc::clone(&receiver);
            let remote = Arc::clone(&remote);
            let root = root.clone();
            let pending = Arc::clone(&pending);
            let worker = std::thread::Builder::new()
                .name("cargo-cache-upload".to_owned())
                .spawn(move || {
                    loop {
                        // Hold the receiver lock only while waiting for work.
                        let next = receiver.lock().recv();
                        let Ok(upload) = next else {
                            break;
                        };
                        if remote.usable()
                            && let Err(error) = exchange::publish(
                                &remote.cache,
                                &root,
                                &upload.unit_hash,
                                &upload.entry,
                            )
                        {
                            remote.failed(error);
                        }
                        pending.fetch_sub(1, Ordering::Relaxed);
                    }
                })
                .context("failed to start remote cache upload worker")?;
            workers.push(worker);
        }
        Ok(Self {
            sender,
            workers,
            pending,
        })
    }

    pub fn enqueue(&self, unit_hash: &str, entry: CacheEntry) {
        self.pending.fetch_add(1, Ordering::Relaxed);
        let upload = Upload {
            unit_hash: unit_hash.to_owned(),
            entry,
        };
        if self.sender.send(upload).is_err() {
            self.pending.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    /// Wait for every queued upload to finish.
    pub fn finish(self) {
        drop(self.sender);
        for worker in self.workers {
            let _ = worker.join();
        }
    }
}
