use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::Stream;
use tokio::task::JoinError;

use crate::tasker::Shared;

/// Stream for the [`join_stream()`][crate::Tasker::join_stream()] method.
pub struct JoinStream {
    shared: Pin<Arc<Shared>>,
}

impl JoinStream {
    pub(crate) fn new(shared: Pin<Arc<Shared>>) -> Self {
        Self { shared }
    }
}

impl Stream for JoinStream {
    type Item = Result<(), JoinError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut handles = self.shared.handles.lock();
        match handles.poll_next(cx) {
            Poll::Ready(Some(res)) => Poll::Ready(Some(res)),
            Poll::Ready(None) if self.shared.all_finished() => Poll::Ready(None),
            _ => {
                // Either all the handles have returned already or the stream is pending,
                // but either way there are still outstanding Tasker clones,
                // and more join handles could be added,
                // so we need to save a waker to be notified when a handle
                // is added or a Tasker clone is marked finished.
                // Meanwhile, return Pending.
                handles.set_waker(cx);
                Poll::Pending
            }
        }
    }
}
