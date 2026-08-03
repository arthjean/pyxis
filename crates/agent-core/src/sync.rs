//! Locking helper shared by the crates that hold short-lived state behind a
//! `Mutex`.
//!
//! Every one of them made the same call: a poisoned lock is RECOVERED rather
//! than propagated. Losing the map, the catalog or the cell state because an
//! unrelated thread panicked is worse than reading state that panicking thread
//! left consistent, since nothing here is written across an unwind. The rule is
//! stated once, here, instead of four times in four crates.

use std::sync::{Mutex, MutexGuard};

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    #[allow(clippy::panic, reason = "poisoning a lock requires an actual panic")]
    fn a_poisoned_lock_still_hands_back_its_value() {
        let shared = Arc::new(Mutex::new(7_u32));
        let poisoner = Arc::clone(&shared);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock();
            panic!("poison the lock");
        })
        .join();
        assert!(shared.lock().is_err(), "the lock must really be poisoned");
        assert_eq!(*lock(&shared), 7);
    }
}
