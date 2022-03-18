use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::vec;

use futures_util::stream::FuturesUnordered;
use futures_util::task::noop_waker_ref;
use futures_util::{FutureExt as _, StreamExt};
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::task::{self, JoinError, JoinHandle};

use crate::{JoinStream, Signaller, Stopper};

#[derive(Default)]
pub(crate) struct Handles {
    handles: FuturesUnordered<JoinHandle<()>>,
    waker: Option<Waker>,
}

impl Handles {
    fn new() -> Self {
        Self {
            handles: FuturesUnordered::new(),
            waker: None,
        }
    }

    fn wake(&mut self) {
        self.waker.take().map(Waker::wake);
        // ^ notifies runtime to repoll JoinStream, if any
    }

    fn push(&mut self, handle: JoinHandle<()>) {
        self.handles.push(handle);
        self.wake();
    }

    pub(crate) fn set_waker(&mut self, cx: &Context<'_>) {
        self.waker = Some(cx.waker().clone());
    }

    pub(crate) fn poll_next(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<(), JoinError>>> {
        self.handles.poll_next_unpin(cx)
    }
}

/// Internal data shared by [`Tasker`] as well as [`Stopper`] clones.
pub(crate) struct Shared {
    pub(crate) handles: Mutex<Handles>,
    /// Number of outstanding `Tasker` clones.
    // NB. We can't use Arc's strong count as `Stopper` also needs a (strong) clone.
    num_clones: AtomicU32,
    finished_clones: AtomicU32,
    pub(crate) stopped: AtomicBool,
    pub(crate) notify_stop: Notify,
}

impl Shared {
    pub(crate) fn new() -> Self {
        Self {
            handles: Mutex::new(Handles::new()),
            num_clones: AtomicU32::new(1),
            finished_clones: AtomicU32::new(0),
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

    pub(crate) fn all_finished(&self) -> bool {
        self.finished_clones.load(Ordering::SeqCst) == self.num_clones.load(Ordering::SeqCst)
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

/// Manages a group of tasks.
///
/// _See [library-level][crate] documentation._
pub struct Tasker {
    shared: Pin<Arc<Shared>>,
}

impl Tasker {
    /// Constructs and empty task set.
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
    /// The `Stopper` future can be used with our [`.unless()`]
    /// extension to stop `Future`s, with [`.take_until()`] to stop `Stream`s,
    /// as part of `tokio::select!()`, and similar...
    ///
    /// [`.unless()`]: crate::FutureExt::unless()
    /// [`.take_until()`]: futures_util::stream::StreamExt::take_until()
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
        self.shared.handles.lock().handles.len()
    }

    fn mark_finished(&self) {
        self.shared.finished_clones.fetch_add(1, Ordering::SeqCst);
        self.shared.handles.lock().wake();
    }

    /// Join all the tasks in the group.
    ///
    /// **This function will panic if any of the tasks panicked**.
    /// Use [`try_join()`][Tasker::try_join()] if you need to handle task panics yourself.
    ///
    /// Note that `join()` will only return once all other `Tasker` clones are finished
    /// (via [`finish()`][Tasker::finish()] or drop).
    pub async fn join(self) {
        let mut join_stream = self.join_stream();
        while let Some(handle) = join_stream.next().await {
            handle.expect("Join error");
        }
    }

    /// Join all the tasks in the group, returning a vector of join results
    /// (ie. results of tokio's [`JoinHandle`][tokio::task::JoinHandle]).
    ///
    /// Note that `try_join()` will only return once all other `Tasker` clones are finished
    /// (via [`finish()`][Tasker::finish()] or drop).
    pub async fn try_join(self) -> Vec<Result<(), JoinError>> {
        self.join_stream().collect().await
    }

    /// Returns a `Stream` which yields join results as they become available,
    /// ie. as the tasks terminate.
    ///
    /// If any of the tasks terminates earlier than others, such as due to a panic,
    /// its result should be readily avialable through this stream.
    ///
    /// The join stream stops after all tasks are joined *and* all other `Tasker`
    /// clones are finished (via [`finish()`][Tasker::finish()] or drop).
    pub fn join_stream(self) -> JoinStream {
        JoinStream::new(self.shared.clone())
        // self is dropped and marked finished
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
        while let Poll::Ready(Some(result)) = handles.handles.poll_next_unpin(&mut cx_noop) {
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

        while let Poll::Ready(Some(result)) = handles.handles.poll_next_unpin(&mut cx_noop) {
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
