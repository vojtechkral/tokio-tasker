use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::vec;

use futures_util::stream::FuturesUnordered;
use futures_util::task::noop_waker_ref;
use futures_util::{FutureExt as _, StreamExt};
use parking_lot::Mutex;
use tokio::sync::{Notify, Semaphore};
use tokio::task::{self, JoinError, JoinHandle};

use crate::{JoinStream, Signaller, Stopper};

/// Internal data shared by [`Tasker`] as well as [`Stopper`] clones.
pub(crate) struct Shared {
    handles: Mutex<FuturesUnordered<JoinHandle<()>>>,
    /// Number of outstanding `Tasker` clones.
    // NB. We can't use Arc's strong count as `Stopper` also needs a (strong) clone.
    num_clones: AtomicU32,
    finished_clones: Semaphore,
    pub(crate) stopped: AtomicBool,
    pub(crate) notify_stop: Notify,
}

impl Shared {
    pub(crate) fn new() -> Self {
        Self {
            handles: Mutex::new(FuturesUnordered::new()),
            num_clones: AtomicU32::new(1),
            finished_clones: Semaphore::new(0),
            stopped: AtomicBool::new(false),
            notify_stop: Notify::new(),
        }
    }

    /// Returns `true` if this call signalled stopping or `false`
    /// if the [`Tasker`] was already stopped.
    pub(crate) fn stop(&self) -> bool {
        let stop = !self.stopped.swap(true, Ordering::SeqCst);
        if stop {
            self.notify_stop.notify_waiters();
        }

        stop
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

/// Manages a group of tasks.
///
/// _See [library-level][self] documentation._
pub struct Tasker {
    shared: Pin<Arc<Shared>>,
}

impl Tasker {
    /// Constructor.
    pub fn new() -> Self {
        Self {
            shared: Arc::pin(Shared::new()),
        }
    }

    /// Add a tokio task handle to the task group.
    ///
    /// It is your responsibility to make sure the task
    /// is stopped by this group's [`Stopper`] future.
    pub fn add_handle(&self, handle: JoinHandle<()>) {
        self.shared.handles.lock().push(handle);
    }

    /// Spawn a `!Send` future the local task set and add its handle to the task group.
    ///
    /// It is your responsibility to make sure the task
    /// is stopped by this group's [`Stopper`] future.
    pub fn spawn<F>(&self, future: F)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = task::spawn(future.map(|_| ()));
        self.add_handle(handle);
    }

    /// Spawn a tokio task and add its handle to the task group.
    /// See [`tokio::task::spawn_local()`].
    ///
    /// It is your responsibility to make sure the task
    /// is stopped by this group's [`Stopper`] future.
    pub fn spawn_local<F>(&self, future: F)
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let handle = task::spawn_local(future.map(|_| ()));
        self.add_handle(handle);
    }

    /// Dispense a [`Stopper`], a future that will resolve once `.stop()`
    /// is called on any `Tasker` (or signaller [`Signaller`]) clone.
    ///
    /// The `Stopper` future can be used with our [`.unless()`][FutureExt::unless()]
    /// extension to stop `Future`s, with [`.take_until()`][futures_util::stream::StreamExt::take_until()]
    /// to stop `Stream`s, as part of `tokio::select!()`, and similar...
    pub fn stopper(&self) -> Stopper {
        Stopper::new(&self.shared)
    }

    /// Stop the tasks in the group.
    ///
    /// This will resolve all [`Stopper`] futures (including ones obtained after this call).
    ///
    /// Returns `true` if this was the first effective stop call
    /// or `false` if the group was already signalled to stop.
    pub fn stop(&self) -> bool {
        self.shared.stop()
    }

    /// Dispense a [`Signaller`], a `Tasker` clone which can be used to signal stopping,
    /// but doesn't require dropping or calling [`finish()`][Tasker::finish()].
    pub fn signaller(&self) -> Signaller {
        let arc = Pin::into_inner(self.shared.clone());
        let weak = Arc::downgrade(&arc);
        Signaller::new(weak)
    }

    /// `true` if stopping was already signalled.
    pub fn is_stopped(&self) -> bool {
        self.shared.stopped.load(Ordering::SeqCst)
    }

    /// Number of tasks currently belonging to this task group.
    pub fn num_tasks(&self) -> usize {
        self.shared.handles.lock().len()
    }

    fn mark_finished(&self) {
        self.shared.finished_clones.add_permits(1);
    }

    /// Wait until all clones are finished.
    async fn await_finished(&self) {
        if self.shared.finished_clones.is_closed() {
            return;
        }

        self.mark_finished();

        let clones = self.shared.num_clones.load(Ordering::SeqCst);
        if self
            .shared
            .finished_clones
            .acquire_many(clones)
            .await
            .is_ok()
        {
            // Mark all as finished by closing the sema
            self.shared.finished_clones.close();
        }
    }

    fn take_handles(&self) -> FuturesUnordered<JoinHandle<()>> {
        let mut lock = self.shared.handles.lock();
        mem::take(&mut *lock)
    }

    /// Wait until all `Tasker` clones are finished (via [`finish()`][Tasker::finish()] or drop)
    /// and then join all the tasks in the group.
    ///
    /// **This function will panic if any of the tasks panicked**.
    /// Use [`try_join()`][Tasker::try_join()] if you need to handle task panics yourself.
    pub async fn join(self) {
        self.await_finished().await;

        let mut handles = self.take_handles();
        while let Some(handle) = handles.next().await {
            handle.expect("Join error");
        }
    }

    /// Wait until all `Tasker` clones are finished (via [`finish()`][Tasker::finish()] or drop)
    /// and then join all the tasks in the group.
    ///
    /// Returns a vector of join results (ie. results of tokio's [`JoinHandle`][tokio::task::JoinHandle]).
    pub async fn try_join(self) -> Vec<Result<(), JoinError>> {
        self.await_finished().await;

        let handles = self.take_handles();
        handles.collect().await
    }

    /// Returns a `Stream` which yields join results as they become available,
    /// ie. as the tasks terminate.
    ///
    /// If any of the tasks terminates earlier than others, such as due to a panic,
    /// its result should be readily avialable through this stream.
    ///
    /// Note that just like with `join()` and `try_join()`, the stream will first
    /// wait until all `Tasker` clones are finished (via [`finish()`][Tasker::finish()] or drop)
    /// before yielding results.
    pub fn join_stream(self) -> JoinStream {
        let handles = self.take_handles();
        let await_finished: Box<dyn Future<Output = ()>> = Box::new(async move {
            self.await_finished().await;
        });

        JoinStream {
            await_finished: Pin::from(await_finished).fuse(),
            handles,
        }
    }

    /// Mark this `Tasker` clone as finished.
    ///
    /// This has the same effect as dropping the clone, but is more explicit.
    /// This lets the task group know that no new tasks will be added through this clone.
    ///
    /// All `Tasker` clones need to be finished/dropped in order for [`.join()`][Tasker::join()]
    /// to be able to join all the tasks.
    ///
    /// Use [`.signaller()`][Tasker::signaller()] to get a special `Tasker` clone that doesn't
    /// need to be dropped and can be used to `.stop()` the task group.
    pub fn finish(self) {
        mem::drop(self); // Drop impl will call mark_finished()
    }

    /// Poll tasks once and join those that are finished.
    ///
    /// Handles to tasks that are finished executing will be
    /// removed from the internal storage.
    ///
    /// Returns the number of tasks that were joined.
    ///
    /// **This function will panic if any of the tasks panicked**.
    /// Use [`try_poll_join()`][Tasker::try_poll_join()] if you need to handle task panics yourself.
    pub fn poll_join(&self) -> usize {
        let mut handles = self.shared.handles.lock();
        let mut cx_noop = Context::from_waker(noop_waker_ref());

        let mut num_joined = 0;
        while let Poll::Ready(Some(result)) = handles.poll_next_unpin(&mut cx_noop) {
            result.expect("Join error");
            num_joined += 1;
        }

        num_joined
    }

    /// Poll tasks once and join those that are already done.
    ///
    /// Handles to tasks that are already finished executing will be joined
    /// and removed from the internal storage.
    ///
    /// Returns vector of join results of tasks that were joined
    /// (may be empty if no tasks could've been joined).
    pub fn try_poll_join(&self) -> Vec<Result<(), JoinError>> {
        let mut handles = self.shared.handles.lock();

        let mut cx_noop = Context::from_waker(noop_waker_ref());
        let mut ready_results = vec![];

        while let Poll::Ready(Some(result)) = handles.poll_next_unpin(&mut cx_noop) {
            ready_results.push(result);
        }

        ready_results
    }
}

impl Default for Tasker {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for Tasker {
    fn clone(&self) -> Self {
        self.shared.num_clones.fetch_add(1, Ordering::SeqCst);

        Self {
            shared: self.shared.clone(),
        }
    }
}

impl Drop for Tasker {
    fn drop(&mut self) {
        self.mark_finished();
    }
}
