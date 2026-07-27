//! Steering queue of the turn that is running (US-007).
//!
//! A steer is neither a new turn nor an interruption. It is an input the actor
//! accepted, persisted and handed to the ALREADY RUNNING turn, which the engine
//! consumes at its next safe point.
//!
//! Ownership is what makes the race arbitrable: the actor is the only producer
//! and the engine the only consumer, so an input is either still queued (and the
//! actor can re-admit it as a new turn when the turn ends without consuming it)
//! or already taken (and it is in the transcript). Never both.

use std::collections::VecDeque;
use std::sync::Mutex;

use agent_core::input::InputQueue;
use agent_core::message::Message;
use tokio::sync::Notify;

/// Inputs accepted for one turn, in acceptance order.
#[derive(Debug, Default)]
pub struct TurnInputs {
    queue: Mutex<VecDeque<String>>,
    waiting: Notify,
}

impl TurnInputs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues an input and wakes a sampling that is waiting on it. Returns the
    /// new depth so the actor can publish it as pending state.
    pub fn push(&self, text: String) -> usize {
        let depth = {
            let mut queue = self.lock();
            queue.push_back(text);
            queue.len()
        };
        // `notify_one` and not `notify_waiters`: it stores a permit when nobody
        // is parked yet, so an input queued between two samplings still wakes the
        // next `ready()` instead of being missed.
        self.waiting.notify_one();
        depth
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Takes what the engine never consumed. Called by the actor AFTER the turn
    /// task returned, so no consumer can be racing with it.
    pub fn drain_remaining(&self) -> Vec<String> {
        self.lock().drain(..).collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<String>> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait::async_trait]
impl InputQueue for TurnInputs {
    fn take(&self) -> Vec<Message> {
        self.lock().drain(..).map(Message::user).collect()
    }

    async fn ready(&self) {
        loop {
            if !self.is_empty() {
                return;
            }
            self.waiting.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn inputs_are_taken_once_in_acceptance_order() {
        let inputs = TurnInputs::new();
        assert_eq!(inputs.push("un".into()), 1);
        assert_eq!(inputs.push("deux".into()), 2);

        let taken: Vec<String> = inputs.take().iter().map(|m| m.text()).collect();
        assert_eq!(taken, vec!["un".to_string(), "deux".to_string()]);
        assert!(inputs.take().is_empty(), "a taken input never comes back");
        assert_eq!(inputs.len(), 0);
    }

    #[tokio::test]
    async fn ready_resolves_for_an_input_queued_before_anyone_waits() {
        let inputs = TurnInputs::new();
        inputs.push("déjà là".into());
        tokio::time::timeout(Duration::from_millis(100), inputs.ready())
            .await
            .expect("an input queued before the wait must still wake it");
    }

    #[tokio::test]
    async fn ready_stays_pending_on_an_empty_queue_then_wakes() {
        let inputs = Arc::new(TurnInputs::new());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), inputs.ready())
                .await
                .is_err(),
            "an empty queue never wakes a sampling"
        );

        let producer = Arc::clone(&inputs);
        tokio::spawn(async move { producer.push("corrige".into()) });
        tokio::time::timeout(Duration::from_secs(1), inputs.ready())
            .await
            .expect("a queued input wakes the sampling");
    }

    #[tokio::test]
    async fn what_the_engine_never_consumed_can_be_re_admitted() {
        let inputs = TurnInputs::new();
        inputs.push("un".into());
        inputs.push("deux".into());
        assert_eq!(inputs.drain_remaining(), vec!["un", "deux"]);
        assert!(inputs.is_empty());
    }
}
