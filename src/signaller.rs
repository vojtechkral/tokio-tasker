use std::sync::Weak;

use crate::tasker::Shared;

/// A weak [`Tasker`] clone which can be used to signal stopping just like `Tasker`,
/// but doesn't require dropping or calling [`finish()`]. Meant to be used from signal
/// handlers or various other callbaks.
///
/// [`Tasker`]: crate::Tasker
/// [`finish()`]: crate::Tasker::finish()
#[derive(Clone)]
pub struct Signaller {
    shared: Weak<Shared>,
}

impl Signaller {
    pub(crate) fn new(shared: Weak<Shared>) -> Self {
        Self { shared }
    }

    /// Stop the tasks in the group.
    ///
    /// This will resolve all [`Stopper`] futures (including ones obtained after this call).
    ///
    /// Returns `true` if this was the first effective stop call
    /// or `false` if the group was already signalled to stop.
    ///
    /// [`Stopper`]: crate::Stopper
    pub fn stop(&self) -> bool {
        if let Some(shared) = self.shared.upgrade() {
            shared.stop()
        } else {
            false
        }
    }
}
