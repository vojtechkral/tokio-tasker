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
//! Finally, there is [`join_stream()`][Tasker::join_stream()] which lets you asynchronously receive task results
//! as they become available, ie. as tasks terminate.
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

mod future_ext;
mod join_stream;
mod signaller;
mod stopper;
mod tasker;

pub use future_ext::{FutureExt, Unless};
pub use join_stream::JoinStream;
pub use signaller::Signaller;
pub use stopper::{Stopped, Stopper};
pub use tasker::Tasker;
