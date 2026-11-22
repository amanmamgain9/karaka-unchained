//! The live write path: the batching window, flush as the barrier, and a
//! failed batch kept for the next drain. (dsh: `live-write-contract.ts`,
//! the cases that belong to the writer; the routing cases run against a
//! real backend in `karaka-session-jsonl`.)
//!
//! Time is paused, so the window is exact.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use karaka_session::{Entry, Seq, SessionEvent, SessionHeader, SessionId, Time};
use karaka_session_persistence::{
    BATCH_WINDOW, LiveWriter, PersistenceError, PersistenceFuture, SessionAccess, SessionHandle,
};

/// What the handle was asked to do.
#[derive(Default)]
struct Calls {
    batches: Vec<Vec<u64>>,
    flushes: usize,
    closed: bool,
    fail_next: bool,
}

/// A write handle that keeps everything in memory.
struct MemoryHandle {
    id: SessionId,
    header: SessionHeader,
    calls: Arc<Mutex<Calls>>,
}

impl MemoryHandle {
    fn new() -> (Self, Arc<Mutex<Calls>>) {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let id = SessionId::new("live");
        let handle = Self {
            header: SessionHeader::new(id.clone(), 1),
            id,
            calls: Arc::clone(&calls),
        };
        (handle, calls)
    }
}

impl SessionHandle for MemoryHandle {
    fn id(&self) -> &SessionId {
        &self.id
    }

    fn header(&self) -> &SessionHeader {
        &self.header
    }

    fn inherited_event_count(&self) -> u64 {
        0
    }

    fn access(&self) -> SessionAccess {
        SessionAccess::Write
    }

    fn read(&self, _: usize, _: Option<usize>) -> PersistenceFuture<'_, Vec<Entry>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn append(&self, events: Vec<Entry>) -> PersistenceFuture<'_, ()> {
        Box::pin(async move {
            let mut calls = self.calls.lock().unwrap();
            if std::mem::take(&mut calls.fail_next) {
                return Err(PersistenceError::Other("simulated write failure".into()));
            }
            calls
                .batches
                .push(events.iter().map(|entry| entry.seq.0).collect());
            Ok(())
        })
    }

    fn flush(&self) -> PersistenceFuture<'_, ()> {
        Box::pin(async move {
            self.calls.lock().unwrap().flushes += 1;
            Ok(())
        })
    }

    fn close(&self) -> PersistenceFuture<'_, ()> {
        Box::pin(async move {
            self.calls.lock().unwrap().closed = true;
            Ok(())
        })
    }
}

fn entry(seq: u64) -> Entry {
    Entry {
        seq: Seq(seq),
        time: Time(1),
        ignorable: false,
        event: SessionEvent::turn_start(1),
    }
}

/// Lets the writer's timer task run.
async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn events_are_written_as_one_batch_when_the_window_ends() {
    let (handle, calls) = MemoryHandle::new();
    let writer = LiveWriter::new(Box::new(handle));
    writer.enqueue(&entry(0));
    writer.enqueue(&entry(1));
    tokio::time::advance(BATCH_WINDOW - Duration::from_millis(1)).await;
    settle().await;
    // Still inside the window.
    writer.enqueue(&entry(2));
    assert!(calls.lock().unwrap().batches.is_empty());

    tokio::time::advance(Duration::from_millis(2)).await;
    settle().await;
    assert_eq!(calls.lock().unwrap().batches, [vec![0, 1, 2]]);
    assert_eq!(writer.pending(), 0);
}

#[tokio::test(start_paused = true)]
async fn flush_writes_at_once_and_flushes_the_handle() {
    let (handle, calls) = MemoryHandle::new();
    let writer = LiveWriter::new(Box::new(handle));
    writer.enqueue(&entry(0));
    writer.flush().await.unwrap();
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.batches, [vec![0]]);
        assert_eq!(calls.flushes, 1);
    }
    // The window found nothing left to write.
    tokio::time::advance(BATCH_WINDOW * 2).await;
    settle().await;
    assert_eq!(calls.lock().unwrap().batches.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_failed_batch_stays_in_order_and_quiets_the_window_until_the_next_drain() {
    let (handle, calls) = MemoryHandle::new();
    let writer = LiveWriter::new(Box::new(handle));
    calls.lock().unwrap().fail_next = true;
    writer.enqueue(&entry(0));
    tokio::time::advance(BATCH_WINDOW).await;
    settle().await;
    // The background write failed; the event waits.
    assert!(calls.lock().unwrap().batches.is_empty());
    assert_eq!(writer.pending(), 1);

    // No new window starts while a failure is pending.
    writer.enqueue(&entry(1));
    tokio::time::advance(BATCH_WINDOW * 2).await;
    settle().await;
    assert!(calls.lock().unwrap().batches.is_empty());

    // The next drain writes both, in order, once.
    writer.flush().await.unwrap();
    assert_eq!(calls.lock().unwrap().batches, [vec![0, 1]]);
    // And the window works again.
    writer.enqueue(&entry(2));
    tokio::time::advance(BATCH_WINDOW).await;
    settle().await;
    assert_eq!(calls.lock().unwrap().batches, [vec![0, 1], vec![2]]);
}

#[tokio::test(start_paused = true)]
async fn flush_reports_a_failure_and_keeps_the_events() {
    let (handle, calls) = MemoryHandle::new();
    let writer = LiveWriter::new(Box::new(handle));
    writer.enqueue(&entry(0));
    calls.lock().unwrap().fail_next = true;
    let failure = writer.flush().await.unwrap_err();
    assert_eq!(failure.to_string(), "simulated write failure");
    assert_eq!(calls.lock().unwrap().flushes, 0);
    assert_eq!(writer.pending(), 1);
    writer.flush().await.unwrap();
    assert_eq!(calls.lock().unwrap().batches, [vec![0]]);
}

#[tokio::test(start_paused = true)]
async fn close_drains_then_closes_and_closes_even_when_the_drain_fails() {
    let (handle, calls) = MemoryHandle::new();
    let writer = LiveWriter::new(Box::new(handle));
    writer.enqueue(&entry(0));
    writer.close().await.unwrap();
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.batches, [vec![0]]);
        assert!(calls.closed);
    }

    let (handle, calls) = MemoryHandle::new();
    let writer = LiveWriter::new(Box::new(handle));
    writer.enqueue(&entry(0));
    calls.lock().unwrap().fail_next = true;
    let failure = writer.close().await.unwrap_err();
    assert_eq!(failure.to_string(), "simulated write failure");
    // Ownership is given up all the same.
    assert!(calls.lock().unwrap().closed);
}
