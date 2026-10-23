//! Task spawning for the async runtime the server runs on.

use std::future::Future;

pub fn spawn<F: Future<Output = ()> + Send + 'static>(future: F) {
    tokio::spawn(future);
}

pub async fn spawn_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    tokio::task::spawn_blocking(work).await
}

pub async fn yield_now() {
    tokio::task::yield_now().await;
}
