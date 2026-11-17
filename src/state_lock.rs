//! Fail-closed locking for actor, configuration and authentication state.
//!
//! Unlike SQLite transactions (which roll back on unwind), these structures
//! have no general recovery contract after a panic. Preserve poisoning and
//! fail the current operation/task rather than exposing partially changed state.

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

pub(crate) fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    #[expect(
        clippy::expect_used,
        reason = "Poisoned actor/auth state has no recovery contract; failing the task prevents reuse of partial state."
    )]
    let guard = state.lock().expect("actor state lock poisoned");
    guard
}
pub(crate) fn read<T>(state: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    #[expect(
        clippy::expect_used,
        reason = "Poisoned configuration/auth state must not be exposed to readers."
    )]
    let guard = state.read().expect("state read lock poisoned");
    guard
}
pub(crate) fn write<T>(state: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    #[expect(
        clippy::expect_used,
        reason = "Poisoned configuration/auth state must not accept subsequent writes."
    )]
    let guard = state.write().expect("state write lock poisoned");
    guard
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "The fresh fixture mutex is initially unpoisoned."
    )]
    #[expect(
        clippy::panic,
        reason = "An injected panic poisons the actor-state lock to verify fail-closed reuse."
    )]
    fn poisoned_actor_state_is_not_recovered() {
        let state = Mutex::new(1);
        assert!(
            std::panic::catch_unwind(|| {
                let _guard = state.lock().unwrap();
                panic!("injected actor failure");
            })
            .is_err()
        );
        assert!(std::panic::catch_unwind(|| drop(lock(&state))).is_err());
    }
}
