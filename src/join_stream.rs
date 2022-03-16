use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::future::{Fuse, FusedFuture};
use futures_util::stream::FuturesUnordered;
use futures_util::{pin_mut, ready, Stream};
use pin_project_lite::pin_project;
use tokio::task::{JoinError, JoinHandle};

pin_project! {
    /// Stream for the [`join_stream()`][Tasker::join_stream()] method.
    pub struct JoinStream {
        pub(crate) await_finished: Fuse<Pin<Box<dyn Future<Output = ()>>>>,
        #[pin]
        pub(crate) handles: FuturesUnordered<JoinHandle<()>>,
    }
}

impl Stream for JoinStream {
    type Item = Result<(), JoinError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        let await_finished = this.await_finished;
        let handles = this.handles;

        if !await_finished.is_terminated() {
            pin_mut!(await_finished);
            ready!(await_finished.poll(cx));
        }

        handles.poll_next(cx)
    }
}
