use eyre::Result;
use futures_util::StreamExt;
use tokio::io;
use tokio::net::{TcpListener, TcpStream};

use tokio_stream::wrappers::TcpListenerStream;
use tokio_tasker::{FutureExt as _, Stopper, Tasker};

/// Connection task
async fn handle_connection(connection: TcpStream, stopper: Stopper) {
    let addr = connection.peer_addr().unwrap();
    eprintln!("Accepted connection from {} ...", addr);

    let (mut reader, mut writer) = connection.into_split();
    let res = io::copy(&mut reader, &mut writer).unless(stopper).await;

    match res {
        Ok(Ok(bytes)) => eprintln!("{}: {} bytes echoed", addr, bytes),
        Ok(Err(err)) => eprintln!("{}: connection error: {}", addr, err),
        Err(_) => eprintln!("{}: Shutting down connection task ...", addr),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    const LISTEN_ADDR: &str = "[::]:12345";

    let tasker = Tasker::new();

    let listener = TcpListener::bind(LISTEN_ADDR).await?;
    eprintln!("Listening for connection at {} ...", LISTEN_ADDR);

    // Clone the tasker for the server task and spawn the server task
    let tasker2 = tasker.clone();
    tasker.spawn(async move {
        let mut listener = TcpListenerStream::new(listener)
            .take_until(tasker2.stopper())
            .enumerate();

        while let Some((i, Ok(connection))) = listener.next().await {
            // Spawn a connection task
            tasker2.spawn(handle_connection(connection, tasker2.stopper()));

            // Poll-join tasks every 10k connections, cleaning up the ones that are
            // already finished. This prevents infinite growth of the Tasker memory.
            if (i + 1) % 10_000 == 0 {
                tasker2.poll_join();
            }
        }

        eprintln!("Server task shuting down...");
        // Let the main tasker clone know we won't be adding any new tasks
        tasker2.finish();
    });

    // Get a signaller clone for the crtlc handler
    let signaller = tasker.signaller();
    ctrlc::set_handler(move || {
        // Tell tasks to stop on Ctrl+C
        if signaller.stop() {
            eprintln!("\nStopping...");
        }
    })?;

    // Join all the tasks
    tasker.join().await;
    eprintln!("All tasks joined! Bye ...");

    Ok(())
}
