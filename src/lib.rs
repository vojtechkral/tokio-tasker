//! Lets you stop and join groups of Tokio tasks.
//!
//! This is a small library intended to help with graceful shutdown in programs or services
//! with a number of independent tasks and/or tasks that require shutdown steps.
//!
//! ## Usage
//!
//! The library's entry point is the [`Tasker`] type, which represents a group of tasks.
//!
//! #### Adding tasks
//!
//! Tasks are added to the group with [`spawn()`][Tasker::spawn()], [`spawn_local()`][Tasker::spawn_local()],
//! or [`add_handle()`][Tasker::add_handle()].
//!
//! The `Tasker` may be cloned freely, the clones may be sent to other tasks/threads and tasks can be
//! spawned on the clones as well. However, **each clone** except the 'main' one needs to be dropped to let the 'main' one join all the tasks.
//! A clone can be dropped using [`finish()`][Tasker::finish()], this is recommended for explicitness.
//! This is to avoid race conditions where tasks could be spawned while the `Tasker` is attempting to join.
//!
//! **Warning:** If you continuously add tasks to `Tasker` (such as network connection handlers etc.),
//! over time its internal storage might grow unreasonably, as it needs to keep a handle to each task.
//! To solve this, use [`poll_join()`][Tasker::poll_join()] or [`try_poll_join()`][Tasker::try_poll_join()]
//! regularly (such as every couple thousand connections or so), this will regularly clean up finished tasks
//! from `Tasker`'s storage.
//!
//! #### Stopping tasks
//!
//! The `Tasker` dispenses [`Stopper`]s, small futures that resolve once the task group is stopped.
//! These can be used to wrap individual futures, streams or used in `select!()` etc.,
//! see [`stopper()`][Tasker::stopper()].
//!
//! To signal to the group that all tasks should be stopped, call [`stop()`][Tasker::stop()]
//! on any `Tasker` clone. This will resolve all the `Stopper` instances. Note that this is not racy,
//! you can still obtain additional `Stopper`s (ie. in other threads etc.) and new tasks can still be
//! set up, they will just be stopped right away.
//!
//! Alternatively you can obtain a [`Signaller`] using [`signaller()`][Tasker::signaller]
//! to get a special `Tasker` clone that provides the `.stop()` method as well, but doesn't need to be finished/dropped
//! before tasks are joined.
//!
//! #### Joining the task group
//!
//! There will usually be a 'main' instance of `Tasker` that will be used to join the tasks.
//! (This doesn't have to be the original clone, whichever clone may be used.)
//!
//! Call [`join().await`][Tasker::join()] at the point where you would like to collect the tasks, such as at the end of `main()` or similar.
//! This will first wait for all the other `Tasker` clones to be finished/dropped and then await the join handles
//! of all the tasks, one by one.
//!
//! If any of the tasks panicked, `join()` will propagate the panic. Use [`try_join()`][Tasker::try_join()]
//! to handle the join results yourself.
//!
//! There are also the [`poll_join()`][Tasker::poll_join()] and [`try_poll_join()`][Tasker::try_poll_join()]
//! non-`async` variants which join already finished tasks without waiting, and release memory used by their handles.
//!
//! ## Example
//!
//! A simple case of using `Tasker` in `main()`:
//!
//! ```rust
//! # use std::time::Duration;
//! #
//! # use eyre::Result;
//! # use futures_util::future;
//! # use futures_util::StreamExt;
//! # use tokio::time;
//! # use tokio_stream::wrappers::IntervalStream;
//! # use tokio_tasker::{FutureExt as _, Tasker};
//! #
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let tasker = Tasker::new();
//!
//!     let tasker2 = tasker.clone();
//!     // Spawn a task that will spawn some subtasks.
//!     // It uses the tasker2 clone to do this.
//!     tasker.spawn(async move {
//!         let pending = future::pending::<()>().unless(tasker2.stopper());
//!         tasker2.spawn(pending);
//!
//!         let interval = time::interval(Duration::from_millis(10_000));
//!         let mut interval = IntervalStream::new(interval).take_until(tasker2.stopper());
//!         tasker2.spawn(async move { while let Some(_) = interval.next().await {} });
//!
//!         // We're done spawning tasks on this clone.
//!         tasker2.finish();
//!     });
//!
//!     // Get a Signaller clone for stopping the group in another task
//!     let signaller = tasker.signaller();
//!     tokio::spawn(async move {
//!         // .stop() the task group after 1s.
//!         time::sleep(Duration::from_millis(1_000)).await;
//!         signaller.stop();
//!     });
//!
//!     // Join all the tasks.
//!     tasker.join().await;
//!
//!     Ok(())
//! }
//! ```
//!
//! There is also an example echo server in `examples/echo.rs`.
//!

use std::error::Error;
use std::fmt::{self, Debug, Display};
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::vec;

use futures_util::future::FusedFuture;
use futures_util::task::noop_waker_ref;
use futures_util::FutureExt as _;
use parking_lot::Mutex;
use pin_project_lite::pin_project;
use tokio::sync::futures::Notified;
use tokio::sync::{Notify, Semaphore};
use tokio::task::{self, JoinError, JoinHandle};

/// Internal data shared by [`Tasker`] as well as [`Stopper`] clones.
struct Shared {
    handles: Mutex<Vec<JoinHandle<()>>>,
    /// Number of outstanding `Tasker` clones.
    // NB. We can't use Arc's strong count as `Stopper` also needs a (strong) clone.
    num_clones: AtomicU32,
    finished_clones: Semaphore,
    stopped: AtomicBool,
    notify_stop: Notify,
}

impl Shared {
    pub(crate) fn new() -> Self {
        Self {
            handles: Mutex::new(vec![]),
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

/// A special [`Tasker`] clone which can be used to signal stopping just like `Tasker`,
/// but doesn't require dropping or calling [`finish()`][Tasker::finish()].
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
    pub fn stop(&self) -> bool {
        if let Some(shared) = self.shared.upgrade() {
            shared.stop()
        } else {
            false
        }
    }
}

// TODO: Debug impls

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

    fn take_handles(&self) -> Vec<JoinHandle<()>> {
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
        for handle in handles.drain(..) {
            handle.await.expect("Join error");
        }
    }

    /// Wait until all `Tasker` clones are finished (via [`finish()`][Tasker::finish()] or drop)
    /// and then join all the tasks in the group.
    ///
    /// Returns a vector of join results (ie. results of tokio's [`JoinHandle`][tokio::task::JoinHandle]).
    pub async fn try_join(self) -> Vec<Result<(), JoinError>> {
        self.await_finished().await;

        let mut handles = self.take_handles();
        let mut res = Vec::with_capacity(handles.len());
        for handle in handles.drain(..) {
            res.push(handle.await);
        }

        res
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
    /// removed from the internal storage (which will be shrunk).
    ///
    /// Returns the number of tasks that were joined.
    ///
    /// **This function will panic if any of the tasks panicked**.
    /// Use [`try_poll_join()`][Tasker::try_poll_join()] if you need to handle task panics yourself.
    pub fn poll_join(&self) -> usize {
        let mut lock = self.shared.handles.lock();

        let num_original = lock.len();
        let mut cx_noop = Context::from_waker(noop_waker_ref());

        let pending_handles: Vec<_> = lock
            .drain(..)
            .filter_map(move |mut handle| match handle.poll_unpin(&mut cx_noop) {
                Poll::Ready(result) => {
                    result.expect("Join error");
                    None
                }
                Poll::Pending => Some(handle),
            })
            .collect();

        let num_joined = num_original - pending_handles.len();
        *lock = pending_handles;
        num_joined
    }

    /// Poll tasks once and join those that are already done.
    ///
    /// Handles to tasks that are already finished executing will be joined
    /// and removed from the internal storage (which will be shrunk).
    ///
    /// Returns vector of join results of tasks that were joined
    /// (may be empty if no tasks could've been joined).
    pub fn try_poll_join(&self) -> Vec<Result<(), JoinError>> {
        let mut lock = self.shared.handles.lock();

        let mut cx_noop = Context::from_waker(noop_waker_ref());
        let mut pending_handles = vec![];
        let mut ready_results = vec![];

        for mut handle in lock.drain(..) {
            match handle.poll_unpin(&mut cx_noop) {
                Poll::Ready(result) => ready_results.push(result),
                Poll::Pending => pending_handles.push(handle),
            }
        }

        *lock = pending_handles;
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

// Future extension

pin_project! {
    /// A [`Future`][std::future::Future] for the [`unless()`][FutureExt::unless()] function.
    #[project = UnlessProj]
    #[project_replace = UnlessProjReplace]
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub enum Unless<F, U> {
        Awaiting {
            #[pin] future: F,
            #[pin] unless: U,
        },
        Done,
    }
}

impl<F, U> Unless<F, U> {
    pub(crate) fn new(future: F, unless: U) -> Self {
        Self::Awaiting { future, unless }
    }
}

impl<F, U> Future for Unless<F, U>
where
    F: Future,
    U: Future,
{
    type Output = Result<F::Output, U::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        use UnlessProj::*;

        let output = match self.as_mut().project() {
            Awaiting { future, unless } => {
                if let Poll::Ready(output) = unless.poll(cx) {
                    Poll::Ready(Err(output))
                } else if let Poll::Ready(output) = future.poll(cx) {
                    Poll::Ready(Ok(output))
                } else {
                    return Poll::Pending;
                }
            }
            Done => panic!("Unless polled after it returned `Poll::Ready`"),
        };

        // If we get here, either of the futures became ready and we have the output.
        // Update state and return result:
        self.as_mut().project_replace(Unless::Done);
        output
    }
}

impl<F, U> FusedFuture for Unless<F, U>
where
    F: Future,
    U: Future,
{
    fn is_terminated(&self) -> bool {
        matches!(self, Unless::Done)
    }
}

impl<F, U> Debug for Unless<F, U>
where
    F: Debug,
    U: Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unless::Awaiting { future, unless } => f
                .debug_struct("Unless::Awaiting")
                .field("future", future)
                .field("unless", unless)
                .finish(),
            Unless::Done => write!(f, "Unless::Done"),
        }
    }
}

/// A [`Future`][std::future::Future] extension adding the [`.unless()`][FutureExt::unless()]
/// method that is desinged to be used with a [`Stopper`].
pub trait FutureExt {
    /// Wrap a future to be stopped from resolving if the `unless` future resolves first.
    ///
    /// This is exactly the same operation as future's `.select()`, but better designed
    /// for stopping futures with the [`Stopper`]. (And without `Unpin` requirement.)
    ///
    /// Unlike `Select`, this future yields a `Result`
    /// where in the `Ok()` variant the result of the original future is returned,
    /// and in the `Err()` variant the result of the `unless` future
    /// is returned in case the `unless` future resolved first.
    ///
    /// When used with [`Stopper`], the future will yield [`Err(Stopped)`][Stopped]
    /// if it is stopped by the associated [`Tasker`].
    fn unless<U>(self, unless: U) -> Unless<Self, U>
    where
        Self: Sized,
        U: Future;
}

impl<F> FutureExt for F
where
    F: Future,
{
    fn unless<U>(self, unless: U) -> Unless<Self, U>
    where
        Self: Sized,
        U: Future,
    {
        Unless::new(self, unless)
    }
}

#[cfg(test)]
mod tests;
