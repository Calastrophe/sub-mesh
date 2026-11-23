//! Waiting for the operating system to ask this process to stop.

/// Resolve when the process is asked to stop.
///
/// On Unix that is Ctrl-C (`SIGINT`) or `SIGTERM`. On Windows it is Ctrl-C,
/// Ctrl-Break, closing the console window, or a logoff or shutdown. Windows
/// gives a console program only a few seconds after the last three, so a
/// graceful stop there is best effort.
///
/// A signal that cannot be registered is logged and never fires. The others
/// still work.
pub async fn requested() {
    tokio::select! {
        () = interrupt() => {}
        () = terminate() => {}
    }
}

async fn interrupt() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "cannot listen for ctrl-c");
        std::future::pending::<()>().await;
    }
}

#[cfg(unix)]
async fn terminate() {
    use tokio::signal::unix::{SignalKind, signal};

    match signal(SignalKind::terminate()) {
        Ok(mut signal) => {
            signal.recv().await;
        }
        Err(error) => {
            tracing::warn!(%error, "cannot listen for SIGTERM");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(windows)]
async fn terminate() {
    use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown};

    match (ctrl_break(), ctrl_close(), ctrl_logoff(), ctrl_shutdown()) {
        (Ok(mut break_), Ok(mut close), Ok(mut logoff), Ok(mut shutdown)) => {
            tokio::select! {
                _ = break_.recv() => {}
                _ = close.recv() => {}
                _ = logoff.recv() => {}
                _ = shutdown.recv() => {}
            }
        }
        _ => {
            tracing::warn!("cannot listen for the console stop events");
            std::future::pending::<()>().await;
        }
    }
}
