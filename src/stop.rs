use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default)]
pub(crate) struct Stop {
    deadline: Option<(Instant, Duration)>,
    cancel: Option<Arc<AtomicBool>>,
}

impl Stop {
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
