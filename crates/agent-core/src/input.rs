//! Steering inputs accepted for the turn that is already running (US-007).
//!
//! A steer is neither a new turn nor an interruption: it is an input the user
//! accepted for the CURRENT turn, which must reach the model at the next safe
//! point instead of waiting for an artificial end of turn.
//!
//! The loop owns the safe points and is the only consumer of this queue. It
//! drains it right before building a request, so an input enters the transcript
//! exactly once, in acceptance order, and never in the middle of a sampling.

use crate::message::Message;

#[async_trait::async_trait]
pub trait InputQueue: Send + Sync {
    /// Takes every input accepted so far, in acceptance order, and leaves the
    /// queue empty. Called ONLY at a safe point, and by a single consumer: what
    /// is taken here is what enters the transcript.
    fn take(&self) -> Vec<Message>;

    /// Resolves as soon as an input is waiting. The loop selects on it while a
    /// sampling is streaming, so an accepted steer cuts that sampling short
    /// instead of being answered one turn late.
    async fn ready(&self);
}
