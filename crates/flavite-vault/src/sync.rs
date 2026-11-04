//! Poison-recovering lock acquisition for the Session/actor tier.
//!
//! Every value guarded by a `Mutex`/`RwLock` in this tier is a std
//! collection or a small scalar: panic-unwind leaves them memory-safe, and
//! the post-poison state means "the interrupted operation did not happen",
//! not "the structure is invalid". Recovering instead of cascading keeps a
//! long-lived process operable after one transient bug; the permanent wedge
//! that unwrapping a poisoned `lock()` causes after any panic-while-held is
//! strictly worse for a GUI host.
//!
//! Recovery is silent to the caller (the guard is returned) and loud to the
//! operator: every recovery logs at `error` (call site via
//! `#[track_caller]`) and bumps [`POISON_RECOVERIES`]. Torn multi-mutex transitions are contained and
//! evented at the turn boundary (`catch_unwind` around the Session turn
//! entry points).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Process-local count of poison recoveries. Lock sites outside a Session
/// context increment this directly; the Session mirrors it into its own
/// counter at drain time so the event channel stays the single
/// observability surface.
pub static POISON_RECOVERIES: AtomicU64 = AtomicU64::new(0);

// Note: `std::sync::PoisonError` exposes only `into_inner` — the original
// panic payload is not retrievable from the public API — so recovery
// logging carries the call site (the actionable half: which lock, where).
#[track_caller]
fn note_recovery(kind: &str) {
    tracing::error!(
        location = %std::panic::Location::caller(),
        "{kind} poisoned; recovering (interrupted operation did not complete)",
    );
    POISON_RECOVERIES.fetch_add(1, Ordering::Relaxed);
}

/// Lock a `Mutex`, recovering from poison instead of cascading a panic.
///
/// Rationale: every value guarded by a `Mutex` in the Session/actor tier is
/// a std collection or a small scalar. Panic-unwind leaves them
/// memory-safe; the post-poison state is "the interrupted operation did not
/// happen", not "the structure is invalid". Multi-step transitions (flight
/// removal + completion push) are covered by the containment boundary at
/// the turn entry points, which marks the session degraded so a torn
/// transition is never silently trusted.
pub trait MutexExt<T> {
    /// Acquire the lock; if poisoned, log, count, and return the inner
    /// guard rather than panicking.
    fn lock_recovering(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    #[track_caller]
    fn lock_recovering(&self) -> MutexGuard<'_, T> {
        match self.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                note_recovery("mutex");
                PoisonError::into_inner(poisoned)
            }
        }
    }
}

/// Read or write an `RwLock`, recovering from poison instead of cascading
/// a panic. Same rationale as [`MutexExt`].
pub trait RwLockExt<T> {
    /// Acquire a read lock; if poisoned, log, count, and return the inner
    /// guard rather than panicking.
    fn read_recovering(&self) -> RwLockReadGuard<'_, T>;
    /// Acquire a write lock; if poisoned, log, count, and return the inner
    /// guard rather than panicking.
    fn write_recovering(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
    #[track_caller]
    fn read_recovering(&self) -> RwLockReadGuard<'_, T> {
        match self.read() {
            Ok(guard) => guard,
            Err(poisoned) => {
                note_recovery("rwlock read");
                PoisonError::into_inner(poisoned)
            }
        }
    }

    #[track_caller]
    fn write_recovering(&self) -> RwLockWriteGuard<'_, T> {
        match self.write() {
            Ok(guard) => guard,
            Err(poisoned) => {
                note_recovery("rwlock write");
                PoisonError::into_inner(poisoned)
            }
        }
    }
}

/// Serialize poison-injecting tests across the crate: they all move the
/// process-global `POISON_RECOVERIES`, and lib tests run on many threads,
/// so exact per-test deltas are only honest under this shared lock.
#[cfg(test)]
pub(crate) fn poison_test_serial() -> &'static Mutex<()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    &SERIAL
}

/// Deliberately poison a `Mutex` by panicking while holding it. Test-only
/// injection for NFR-2 proofs; keep the caller's `poison_test_serial`
/// guard held across the whole test.
#[cfg(test)]
pub(crate) fn poison_mutex_for_test<T>(mutex: &Mutex<T>) {
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _held = mutex.lock().unwrap();
        panic!("test-injected panic while holding the lock");
    }));
    assert!(caught.is_err(), "injected panic must unwind the holder");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic;

    /// NFR-2 at the lock level: a panic while holding a lock poisons it;
    /// plain unwrap-lock acquisition would then wedge every later
    /// acquisition in the process, but the extension returns a usable
    /// guard and counts the recovery. Removing the recovery (naive unwrap
    /// body) makes this test panic — it is the honest guard for this file.
    #[test]
    fn poison_is_recovered_and_counted() {
        let _serial = poison_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let before = POISON_RECOVERIES.load(Ordering::Relaxed);

        let m = Mutex::new(vec![7u32]);
        let caught = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _held = m.lock().unwrap();
            panic!("boom while holding the mutex");
        }));
        assert!(caught.is_err());
        {
            let mut g = m.lock_recovering();
            g.push(9);
            assert_eq!(*g, vec![7, 9]);
        }

        let r = RwLock::new(vec![1i8]);
        let caught = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _held = r.write().unwrap();
            panic!("boom while writing");
        }));
        assert!(caught.is_err());
        assert_eq!(*r.read_recovering(), vec![1]);
        r.write_recovering().push(2);
        assert_eq!(*r.read_recovering(), vec![1, 2]);

        assert_eq!(
            POISON_RECOVERIES.load(Ordering::Relaxed),
            before + 4,
            "every recovery must bump the observable counter"
        );
    }
}
