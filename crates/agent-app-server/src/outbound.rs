//! Bounded outbound queue (US-018 AC4).
//!
//! A client that stops reading must cost the server a bounded amount of memory
//! and nothing else: the thread actor keeps running, the durable log keeps
//! being written, and what the queue drops was never the source of truth. Two
//! bounds, because either one alone is escapable: a million empty
//! notifications, or one notification carrying a 200 MiB tool output.
//!
//! Crossing a bound is TERMINAL for the connection, not for the thread: the
//! client is told why, the socket closes, and everything committed is still
//! readable with `thread/items/list` after reconnecting.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Mutex, mpsc};

use crate::jsonrpc::Outbound;

/// Messages a connection may have in flight.
pub const MAX_QUEUED_EVENTS: usize = 1_024;
/// Bytes a connection may have in flight.
pub const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;

/// Why a connection was closed by the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overflow {
    pub detail: String,
}

/// Sending half. Cloneable: the dispatcher, the event pump and the approval
/// path all publish through the same accounting.
#[derive(Clone)]
pub struct Outbox {
    tx: mpsc::Sender<Queued>,
    queued_bytes: Arc<AtomicUsize>,
    overflow: Arc<std::sync::Mutex<Option<Overflow>>>,
}

struct Queued {
    message: Outbound,
    weight: usize,
}

impl Outbox {
    pub fn new() -> (Self, OutboxReceiver) {
        let (tx, rx) = mpsc::channel(MAX_QUEUED_EVENTS);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let overflow = Arc::new(std::sync::Mutex::new(None));
        (
            Self {
                tx,
                queued_bytes: Arc::clone(&queued_bytes),
                overflow: Arc::clone(&overflow),
            },
            OutboxReceiver {
                rx: Mutex::new(rx),
                queued_bytes,
            },
        )
    }

    /// Enqueues a message.
    ///
    /// Never awaits: back-pressure is REFUSAL, not blocking, so a slow client
    /// cannot slow the thread actor down by one microsecond.
    pub fn send(&self, message: Outbound) -> Result<(), Overflow> {
        if let Some(overflow) = self.overflow() {
            return Err(overflow);
        }
        let weight = message.weight();
        let queued = self.queued_bytes.load(Ordering::Acquire);
        if queued.saturating_add(weight) > MAX_QUEUED_BYTES {
            return Err(self.overflowed(format!(
                "client queue exceeded {MAX_QUEUED_BYTES} bytes ({queued} queued, \
                 next message {weight}); reconnect and resume the thread"
            )));
        }
        self.queued_bytes.fetch_add(weight, Ordering::AcqRel);
        match self.tx.try_send(Queued { message, weight }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(queued)) => {
                self.queued_bytes.fetch_sub(queued.weight, Ordering::AcqRel);
                Err(self.overflowed(format!(
                    "client queue exceeded {MAX_QUEUED_EVENTS} pending events; \
                     reconnect and resume the thread"
                )))
            }
            Err(mpsc::error::TrySendError::Closed(queued)) => {
                self.queued_bytes.fetch_sub(queued.weight, Ordering::AcqRel);
                Err(self.overflowed("client connection closed".to_string()))
            }
        }
    }

    pub fn overflow(&self) -> Option<Overflow> {
        self.overflow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Records the FIRST cause. A later overflow does not overwrite the reason
    /// the client is about to be told.
    fn overflowed(&self, detail: String) -> Overflow {
        let mut held = self
            .overflow
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.get_or_insert(Overflow { detail }).clone()
    }
}

/// Receiving half, owned by the writer task.
pub struct OutboxReceiver {
    rx: Mutex<mpsc::Receiver<Queued>>,
    queued_bytes: Arc<AtomicUsize>,
}

impl OutboxReceiver {
    /// Next message to write. `None` once every sender is gone.
    pub async fn recv(&self) -> Option<Outbound> {
        let queued = self.rx.lock().await.recv().await?;
        self.queued_bytes.fetch_sub(queued.weight, Ordering::AcqRel);
        Some(queued.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::RequestId;

    fn message(payload: &str) -> Outbound {
        Outbound::Notification {
            method: "item/agentMessage/delta".into(),
            params: serde_json::json!({ "delta": payload }),
        }
    }

    #[tokio::test]
    async fn a_reading_client_sees_no_bound() {
        let (outbox, receiver) = Outbox::new();
        for _ in 0..(MAX_QUEUED_EVENTS * 2) {
            outbox.send(message("x")).expect("queued");
            receiver.recv().await.expect("drained");
        }
        assert!(outbox.overflow().is_none());
    }

    #[tokio::test]
    async fn a_client_that_stops_reading_is_closed_with_a_cause() {
        let (outbox, _receiver) = Outbox::new();
        let mut sent = 0;
        let overflow = loop {
            match outbox.send(message("x")) {
                Ok(()) => sent += 1,
                Err(overflow) => break overflow,
            }
            assert!(sent <= MAX_QUEUED_EVENTS, "the queue must be bounded");
        };
        assert_eq!(sent, MAX_QUEUED_EVENTS);
        assert!(overflow.detail.contains("pending events"), "{overflow:?}");
        // Past the bound the queue stays refused with the SAME cause.
        assert_eq!(outbox.send(message("x")).expect_err("closed"), overflow);
    }

    #[tokio::test]
    async fn one_oversized_message_closes_the_connection_too() {
        let (outbox, _receiver) = Outbox::new();
        let huge = Outbound::Response {
            id: RequestId::Number(1),
            result: serde_json::json!({ "output": "x".repeat(MAX_QUEUED_BYTES + 1) }),
        };
        let overflow = outbox.send(huge).expect_err("over the byte bound");
        assert!(overflow.detail.contains("bytes"), "{overflow:?}");
    }

    /// The byte bound is on what is IN FLIGHT, not on what a connection sent
    /// over its lifetime: a client that keeps reading streams gigabytes.
    #[tokio::test]
    async fn draining_frees_the_byte_budget() {
        let (outbox, receiver) = Outbox::new();
        let payload = "x".repeat(MAX_QUEUED_BYTES * 3 / 4);
        for _ in 0..4 {
            outbox.send(message(&payload)).expect("queued");
            receiver.recv().await.expect("drained");
        }
        assert!(outbox.overflow().is_none());
    }
}
