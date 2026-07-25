//! `CancelToken` — signal d'annulation COOPÉRATIF de la boucle (US-001).
//!
//! Le cœur est responsable de son propre arrêt : le client signale, la boucle
//! s'arrête à une frontière connue (fin d'événement de stream, retour de
//! dispatch, réveil de backoff) puis réconcilie son transcript. Un
//! `JoinHandle::abort()` côté client couperait le future à un point arbitraire,
//! entre le push d'un `tool_use` et l'écriture de son résultat.
//!
//! Support : `tokio::sync::watch`, déjà disponible via la feature `sync` du
//! workspace — aucune dépendance nouvelle (ADR-3 : `Deps` reste un sac de
//! primitives injectées).

use std::future::Future;
use std::sync::Arc;

use tokio::sync::watch;

/// Signal d'annulation clonable. Émettre et observer se font par le même type :
/// le cœur observe, le client émet, et le `Arc<Sender>` garde le canal ouvert
/// tant qu'un porteur existe (pas de `changed()` en erreur).
#[derive(Clone, Debug)]
pub struct CancelToken {
    tx: Arc<watch::Sender<bool>>,
}

impl CancelToken {
    /// Nouveau signal, non annulé.
    pub fn new() -> Self {
        Self {
            tx: Arc::new(watch::Sender::new(false)),
        }
    }

    /// Signale l'annulation. Idempotent : un signal émis après l'arrêt de la
    /// boucle ne produit ni panique ni effet observable.
    pub fn cancel(&self) {
        // `send_replace` (et non `send`) : un canal sans récepteur vivant — boucle
        // déjà terminée — ne doit pas remonter d'erreur.
        self.tx.send_replace(true);
    }

    /// État courant, sans attente. Utilisé aux frontières de boucle.
    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Se résout quand l'annulation est signalée. Reste en attente sinon.
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        if *rx.borrow_and_update() {
            return;
        }
        while rx.changed().await.is_ok() {
            if *rx.borrow_and_update() {
                return;
            }
        }
        // Émetteur disparu sans annulation : rien ne peut plus arriver.
        std::future::pending::<()>().await
    }

    /// Court `fut` jusqu'à son terme ou jusqu'à l'annulation. Le future est
    /// interrogé EN PREMIER (`biased`) : un travail déjà terminé au moment du
    /// signal rend son résultat réel plutôt que d'être perdu (edge case #2).
    pub async fn guard<F: Future>(&self, fut: F) -> Cancellable<F::Output> {
        tokio::select! {
            biased;
            out = fut => Cancellable::Completed(out),
            () = self.cancelled() => Cancellable::Cancelled,
        }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Issue d'un travail placé sous `CancelToken::guard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cancellable<T> {
    Completed(T),
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_is_observed_by_every_clone() {
        let token = CancelToken::new();
        let observer = token.clone();
        assert!(!observer.is_cancelled());
        token.cancel();
        assert!(observer.is_cancelled());
        observer.cancelled().await;
    }

    #[tokio::test]
    async fn cancel_is_idempotent_and_survives_dropped_observers() {
        let token = CancelToken::new();
        token.cancel();
        // Deuxième signal après « arrêt » : aucun effet, aucune panique.
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn guard_prefers_a_finished_future_over_a_pending_cancel() {
        let token = CancelToken::new();
        token.cancel();
        let out = token.guard(async { 42 }).await;
        assert_eq!(out, Cancellable::Completed(42));
    }

    #[tokio::test]
    async fn guard_cancels_a_future_that_never_completes() {
        let token = CancelToken::new();
        let signal = token.clone();
        tokio::spawn(async move { signal.cancel() });
        let out = token.guard(std::future::pending::<()>()).await;
        assert_eq!(out, Cancellable::Cancelled);
    }
}
