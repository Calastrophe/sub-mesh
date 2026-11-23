//! The writer of the request log.
//!
//! A relayed request is logged twice. When its response starts, the relay
//! queues the row through a [`Slot`], which gives the row its id. When the
//! response ends, the [`Entry`] that the slot returned queues the update.
//! One task writes both to SQLite in the order they were queued, so a handler
//! never waits for the database and an update never reaches a row before its
//! insert.
//!
//! Ids come from a counter that starts at the highest id on disk. A row whose
//! second write never came, because the server stopped, is closed at the next
//! start.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Instant;

use anyhow::Context;
use chrono::Utc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use crate::db::{Completion, Db, RequestRecord};
use crate::registry::WarmthRow;

/// Each unfinished request can hold one start and one finish message.
const MAX_PENDING_REQUESTS: usize = 1024;

enum Message {
    Start {
        record: Box<RequestRecord>,
        warmth: Vec<WarmthRow>,
    },
    Finish {
        id: i64,
        completion: Completion,
        permit: OwnedSemaphorePermit,
    },
}

/// A handle to the writer task. Clones share the task and the id counter.
#[derive(Clone)]
pub struct Log {
    sender: mpsc::Sender<Message>,
    last_id: Arc<AtomicI64>,
    pending: Arc<Semaphore>,
}

impl Log {
    /// Start the writer task on the current runtime. New ids follow the
    /// highest id in `db`. Rows that no response end completed, because the
    /// server stopped first, are closed now.
    pub fn start(db: Db) -> anyhow::Result<Log> {
        let closed = db
            .close_unfinished_requests(Utc::now())
            .context("cannot close the requests that the last run left open")?;
        if closed > 0 {
            tracing::info!(count = closed, "closed requests cut off by a restart");
        }
        let last_id = db
            .last_request_id()
            .context("cannot read the request log")?;
        let (sender, mut receiver) = mpsc::channel(2 * MAX_PENDING_REQUESTS);
        tokio::spawn(async move {
            while let Some(message) = receiver.recv().await {
                if let Err(error) = write(&db, message).await {
                    tracing::warn!(%error, "cannot log request");
                }
            }
        });
        Ok(Log {
            sender,
            last_id: Arc::new(AtomicI64::new(last_id)),
            pending: Arc::new(Semaphore::new(MAX_PENDING_REQUESTS)),
        })
    }

    /// Wait for room to queue one row. `None` when the writer has stopped.
    ///
    /// Send through the slot while holding the lock that orders the
    /// requests, so that ids and writes follow that order.
    pub async fn reserve(&self) -> Option<Slot<'_>> {
        let permit = self.pending.clone().acquire_owned().await.ok()?;
        (!self.sender.is_closed()).then_some(Slot { log: self, permit })
    }

    /// A slot reserves room for both writes through the pending-request limit.
    fn queue(&self, message: Message) {
        if let Err(error) = self.sender.try_send(message) {
            tracing::warn!(%error, "cannot queue request log write");
        }
    }
}

async fn write(db: &Db, message: Message) -> anyhow::Result<()> {
    db.run(move |db| match message {
        Message::Start { record, warmth } => db.record_request(&record, &warmth),
        Message::Finish {
            id,
            completion,
            permit,
        } => {
            let result = db.complete_request(id, &completion);
            drop(permit);
            result
        }
    })
    .await
}

/// Room in the queue for one request row.
pub struct Slot<'a> {
    log: &'a Log,
    permit: OwnedSemaphorePermit,
}

impl Slot<'_> {
    /// Queue `record` and the cache warmth it left. The slot sets the id of
    /// the record. `started` is when the request began, for its duration.
    pub fn send(
        self,
        mut record: RequestRecord,
        warmth: Vec<WarmthRow>,
        started: Instant,
    ) -> Entry {
        let id = self.log.last_id.fetch_add(1, Ordering::SeqCst) + 1;
        record.id = id;
        self.log.queue(Message::Start {
            record: Box::new(record),
            warmth,
        });
        Entry {
            log: self.log.clone(),
            id,
            started,
            permit: Some(self.permit),
        }
    }
}

/// A logged request whose response has not ended yet. It completes the row
/// once. A drop completes it with nothing found but its duration.
pub struct Entry {
    log: Log,
    id: i64,
    started: Instant,
    permit: Option<OwnedSemaphorePermit>,
}

impl Entry {
    /// Record how the response ended. The entry sets the duration.
    pub fn complete(mut self, completion: Completion) {
        self.finish(completion);
    }

    fn finish(&mut self, mut completion: Completion) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        completion.duration_ms =
            u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.log.queue(Message::Finish {
            id: self.id,
            completion,
            permit,
        });
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.finish(Completion::default());
    }
}

#[cfg(test)]
mod tests;
