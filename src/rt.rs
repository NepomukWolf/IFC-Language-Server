//! Task spawning for the async runtime the server runs on: tokio natively, and the browser's
//! single-threaded event loop on wasm32, where blocking work runs inline on the calling task.

use std::future::Future;

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn<F: Future<Output = ()> + Send + 'static>(future: F) {
    tokio::spawn(future);
}

#[cfg(target_arch = "wasm32")]
pub fn spawn<F: Future<Output = ()> + 'static>(future: F) {
    wasm_bindgen_futures::spawn_local(future);
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn spawn_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    tokio::task::spawn_blocking(work).await
}

#[cfg(target_arch = "wasm32")]
pub async fn spawn_blocking<T>(work: impl FnOnce() -> T) -> Result<T, std::convert::Infallible> {
    Ok(work())
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn yield_now() {
    tokio::task::yield_now().await;
}

#[cfg(target_arch = "wasm32")]
pub async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}
