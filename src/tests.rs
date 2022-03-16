use std::time::Duration;

use futures_util::{future, poll, stream, StreamExt};
use tokio::time;

use super::*;

// TODO: Move as int test?

async fn stopable_future(mut stopper: Stopper) -> Result<(), Stopped> {
    let mut unless = future::pending::<()>().unless(&mut stopper);
    (&mut unless).await.unwrap_err();
    assert!(unless.is_terminated()); // FusedFuture impl
    drop(unless);

    assert!(stopper.is_stopped());
    assert!(stopper.is_terminated()); // FusedFuture impl

    let res = stopper.ok_or_stopped(());
    assert!(matches!(res, Err(Stopped)));
    res
}

async fn stopable_stream(stopper: Stopper) {
    let mut stream = stream::pending::<()>().take_until(stopper);
    while let Some(_) = stream.next().await {}
}

async fn yield_to_tokio() {
    for _ in 0..10 {
        task::yield_now().await;
        time::sleep(Duration::from_millis(10)).await;
    }
}

macro_rules! test_st_mt {
    (@ $st_name:ident, $mt_name:ident, $attr:meta, $test:expr) => {
        #[tokio::test(flavor = "current_thread")]
        #[$attr]
        async fn $st_name() {
            $test
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
        #[$attr]
        async fn $mt_name() {
            $test
        }
    };

    ($st_name:ident, $mt_name:ident, $test:expr) => {
        test_st_mt!(@ $st_name, $mt_name, doc(), $test);
    };

    (#[should_panic] $st_name:ident, $mt_name:ident, $test:expr) => {
        test_st_mt!(@ $st_name, $mt_name, should_panic, $test);
    };
}

test_st_mt!(basic_st, basic_mt, {
    let tasker = Tasker::new();

    tasker.spawn(stopable_future(tasker.stopper()));
    tasker.spawn(stopable_stream(tasker.stopper()));

    tasker.stop();
    tasker.join().await;
});

test_st_mt!(many_taskers_st, many_taskers_mt, {
    let tasker = Tasker::new();

    let num_stopped = Arc::new(AtomicU32::new(0));

    for i in 0..100 {
        let tasker = tasker.clone();
        let num_stopped = num_stopped.clone();
        let stopper = tasker.stopper();
        tasker.spawn(async move {
            let _ = stopable_future(stopper).await;
            num_stopped.fetch_add(1, Ordering::SeqCst);
        });

        // Call finish() on even iterations only, this should verify
        // it works the same way as dropping
        if i % 2 == 0 {
            tasker.finish();
        }
    }

    tasker.stop();
    let num_joined = tasker.try_join().await.len();
    assert_eq!(num_joined, 100);

    let num_stopped = num_stopped.load(Ordering::SeqCst);
    assert_eq!(num_stopped, 100);
});

test_st_mt!(
    #[should_panic]
    join_panic_st,
    join_panic_mt,
    {
        let tasker = Tasker::new();

        tasker.spawn(future::pending::<()>());
        tasker.spawn(async { panic!("Things aren't going well") });

        tasker.stop();
        tasker.join().await;
    }
);

test_st_mt!(try_join_st, try_join_mt, {
    let tasker = Tasker::new();

    tasker.spawn(stopable_future(tasker.stopper()));
    tasker.spawn(async { panic!("Things aren't going well") });

    tasker.stop();
    let res = tasker.try_join().await;
    res[0].as_ref().unwrap();
    assert!(res[1].as_ref().unwrap_err().is_panic());
});

test_st_mt!(join_stream_st, join_stream_mt, {
    let tasker = Tasker::new();

    tasker.spawn(future::pending::<()>());
    tasker.spawn(async { panic!("Things aren't going well") });
    let tasker2 = tasker.clone();

    tasker.stop();

    // Assert that the join stream won't start yielding results
    // until all tasker clones are done.
    let mut join_stream = tasker.join_stream();
    assert!(poll!(join_stream.next()).is_pending());

    tasker2.finish();

    // We should get a result now, even if the pending future is still, uh, pending.
    let res = join_stream.next().await.unwrap();
    assert!(res.unwrap_err().is_panic());
});

test_st_mt!(join_stream_issue2_st, join_stream_issue2_mt, {
    // https://github.com/vojtechkral/tokio-tasker/issues/2

    let tasker = Tasker::new();
    let tasker2 = tasker.clone();

    tasker.spawn(async move {
        yield_to_tokio().await;
        // time::sleep(Duration::from_millis(500)).await;
        tasker2.spawn(async {
            panic!("foo");
        });
        tasker2.finish();
    });

    let stream = tasker.join_stream();
    let results: Vec<_> = stream.collect().await;
    assert_eq!(results.len(), 2);
});

test_st_mt!(poll_join_st, poll_join_mt, {
    let tasker = Tasker::new();

    for _ in 0..50 {
        tasker.spawn(future::ready(()));
    }
    for _ in 0..50 {
        tasker.spawn(stopable_future(tasker.stopper()));
    }

    // Make sure the ready tasks are done
    yield_to_tokio().await;

    // The ready tasks should be joinable
    assert_eq!(tasker.poll_join(), 50);

    // Make sure the remaining tasks are done too
    tasker.stop();
    yield_to_tokio().await;

    let mut remaining = tasker.try_poll_join();
    assert_eq!(remaining.len(), 50);
    remaining.drain(..).collect::<Result<(), _>>().unwrap();

    tasker.join().await;
});

test_st_mt!(tasker_not_finished_st, tasker_not_finished_mt, {
    let tasker = Tasker::new();
    let tasker2 = tasker.clone();

    tasker.stop();
    let mut join_ft = Box::pin(tasker.join());

    // Poll the join() future a couple of time and verify it doesn't resolve
    for _ in 0..10 {
        time::sleep(Duration::from_millis(20)).await;
        assert!(matches!(poll!(&mut join_ft), Poll::Pending));
    }

    drop(join_ft); // Contains tasker

    // Now that we've dropped tasker, tasker2 should be joinable
    tasker2.join().await;
});

#[tokio::test]
async fn unless_not_stopped() {
    let dud_stopper = future::pending::<Stopped>();
    let mut unless = future::ready(()).unless(dud_stopper);
    let res = (&mut unless).await;
    assert!(unless.is_terminated());
    assert!(matches!(res, Ok(())));
}
