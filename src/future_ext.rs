use std::fmt::{self, Debug};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::future::FusedFuture;
use pin_project_lite::pin_project;

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
