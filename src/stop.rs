//! When the lookup running on this thread must stop: past its deadline, or
//! cancelled because nobody waits for its answer any more.
//!
//! A lookup runs on a blocking thread, which nothing can interrupt from
//! outside: dropping the future that waits for it drops only the answer, and
//! the thread works on. So the lookup calls [`check`] between its steps and
//! stops itself.
//!
//! The stop is per thread, because every step of a lookup runs on the thread
//! that started it. Work handed to another thread does not see it.

use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::error::{Error, Result};

/// When a lookup must stop. The default never does.
#[derive(Debug, Clone, Default)]
pub(crate) struct Stop {
    deadline: Option<(Instant, Duration)>,
    cancel: Option<Arc<AtomicBool>>,
}

impl Stop {
    /// Stop `timeout` from now, or once `cancel` is set, whichever comes
    /// first.
    pub(crate) fn new(timeout: Duration, cancel: Arc<AtomicBool>) -> Self {
        Self {
            deadline: Some((Instant::now() + timeout, timeout)),
            cancel: Some(cancel),
        }
    }

    fn reached(&self) -> Result<()> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
        {
            return Err(Error::Cancelled);
        }
        match self.deadline {
            Some((at, after)) if Instant::now() >= at => Err(Error::TimedOut { after }),
            _ => Ok(()),
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Stop> = RefCell::new(Stop::default());
}

/// Run `work` on this thread under `stop`, then put back the stop it
/// replaced, also when `work` panics: a batch catches an item's panic and
/// runs the next item on the same thread.
pub(crate) fn within<T>(stop: Stop, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<Stop>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(stop) = self.0.take() {
                CURRENT.set(stop);
            }
        }
    }
    let _restore = Restore(Some(CURRENT.replace(stop)));
    work()
}

/// Fails once the lookup on this thread is past its deadline or cancelled.
pub(crate) fn check() -> Result<()> {
    CURRENT.with_borrow(Stop::reached)
}

/// Sets its flag when dropped. axum drops a handler's future when its client
/// disconnects, and this with it, which stops the work the handler started.
pub(crate) struct CancelOnDrop(pub(crate) Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    #[test]
    fn a_thread_with_no_stop_never_stops() {
        assert!(check().is_ok());
    }

    #[test]
    fn a_lookup_stops_once_past_its_deadline() {
        let stop = Stop::new(Duration::from_millis(1), Arc::new(AtomicBool::new(false)));
        let outcome = within(stop, || {
            std::thread::sleep(Duration::from_millis(5));
            check()
        });
        assert!(matches!(
            outcome,
            Err(Error::TimedOut { after }) if after == Duration::from_millis(1)
        ));
    }

    #[test]
    fn a_lookup_stops_once_cancelled() {
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Stop::new(Duration::from_secs(3600), Arc::clone(&cancel));
        within(stop, || {
            assert!(check().is_ok());
            drop(CancelOnDrop(Arc::clone(&cancel)));
            assert!(matches!(check(), Err(Error::Cancelled)));
        });
    }

    #[test]
    fn the_stop_before_comes_back_after_work_and_after_a_panic() {
        let expired = || Stop::new(Duration::ZERO, Arc::new(AtomicBool::new(false)));
        within(Stop::default(), || {
            within(expired(), || assert!(check().is_err()));
            assert!(check().is_ok());
            let panicked = catch_unwind(AssertUnwindSafe(|| within(expired(), || panic!("item"))));
            assert!(panicked.is_err());
            assert!(check().is_ok());
        });
    }
}
