use std::error::Error;
use std::fmt::{self, Debug, Display};
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::future::FusedFuture;
use pin_project_lite::pin_project;
use tokio::sync::futures::Notified;

use crate::tasker::Shared;

/// An error type for the "stopped by [`Stopper`]" situation.
///
/// May be convenient to bubble task stopping up error chains.
#[derive(Debug)]
pub struct Stopped;

impl Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "The task was stopped")
    }
}

impl Error for Stopped {
    fn description(&self) -> &str {
        "The task was stopped"
    }
}

pin_project! {
    /// A [`Future`] that yields [`Stopped`] once `.stop()` is called on any
    /// handle of the task group (either [`Tasker`] or [`Signaller`]).
    ///
    /// Obtained with [`Tasker::stopper()`].
    ///
    /// [`Tasker`]: crate::Tasker
    /// [`Signaller`]: crate::Signaller
    /// [`Tasker::stopper()`]: crate::Tasker::stopper()
    pub struct Stopper {
        // SAFETY: Drop order matters! `notified` must come before `shared`.
        #[pin] notified: Option<Pin<Box<Notified<'static>>>>,
        shared: Pin<Arc<Shared>>,
    }
}

impl Stopper {
    pub(crate) fn new(shared: &Pin<Arc<Shared>>) -> Self {
        let notified = if shared.stopped.load(Ordering::SeqCst) {
            None
        } else {
            let notified = shared.notify_stop.notified();
            // SAFETY: We're keeping a Pin<Arc> to the referenced value until Self is dropped.
            let notified: Notified<'static> = unsafe { mem::transmute(notified) };
            Some(Box::pin(notified))
        };

        Self {
            shared: shared.clone(),
            notified,
        }
    }

    /// `true` if stopping was already signalled.
    pub fn is_stopped(&self) -> bool {
        self.shared.stopped.load(Ordering::SeqCst)
    }

    /// Convenience function to create a `Result` from a `value`
    /// based on the stopped state. If the stop condition was already signalled,
    /// returns `Err(Stopped)`.
    pub fn ok_or_stopped<T>(&self, value: T) -> Result<T, Stopped> {
        if self.is_stopped() {
            Err(Stopped)
        } else {
            Ok(value)
        }
    }
}

impl Future for Stopper {
    type Output = Stopped;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.shared.stopped.load(Ordering::SeqCst) {
            return Poll::Ready(Stopped);
        }

        let this = self.project();
        match this.notified.as_pin_mut() {
            Some(notified) => notified.poll(cx).map(|_| Stopped),
            None => Poll::Ready(Stopped),
        }
    }
}

impl FusedFuture for Stopper {
    fn is_terminated(&self) -> bool {
        self.is_stopped()
    }
}

impl Clone for Stopper {
    fn clone(&self) -> Self {
        Self::new(&self.shared)
    }
}

impl fmt::Debug for Stopper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stopper")
            .field("tasker", &self.shared.ptr())
            .field("stopped", &self.is_stopped())
            .finish()
    }
}
