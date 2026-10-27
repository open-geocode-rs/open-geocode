//! Graceful shutdown for `serve`.

/// Resolves on Ctrl-C, or on SIGTERM (what Docker, Kubernetes and systemd send to stop a
/// service) on unix. If a signal cannot be listened for, that branch never resolves, so the
/// server keeps running rather than stopping at once.
pub(crate) async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            eprintln!("cannot listen for Ctrl-C: {error}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(error) => {
                eprintln!("cannot listen for SIGTERM: {error}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
