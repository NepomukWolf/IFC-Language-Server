//! Task spawning, timers and the clock for the async runtime the server runs on: tokio natively,
//! and the browser's single-threaded event loop on wasm32, where blocking work runs inline on the
//! calling task.

use std::future::Future;
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
pub use tokio::time::Instant;

#[cfg(not(target_arch = "wasm32"))]
pub fn now() -> Instant {
    Instant::now()
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// Milliseconds since the epoch from `Date.now()`, since `std::time::Instant` panics on
/// wasm32-unknown-unknown. A clock set backwards yields a zero duration.
#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Instant(f64);

#[cfg(target_arch = "wasm32")]
impl std::ops::Sub for Instant {
    type Output = Duration;

    fn sub(self, earlier: Self) -> Duration {
        Duration::from_secs_f64((self.0 - earlier.0).max(0.0) / 1000.0)
    }
}

#[cfg(target_arch = "wasm32")]
pub fn now() -> Instant {
    Instant(js_sys::Date::now())
}

/// Waits on `setTimeout` of the global scope, which exists in windows and workers alike.
#[cfg(target_arch = "wasm32")]
pub async fn sleep(duration: Duration) {
    use wasm_bindgen::{JsCast, JsValue};

    let timer = js_sys::Promise::new(&mut |resolve, _reject| {
        let set_timeout = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("setTimeout"))
            .ok()
            .and_then(|function| function.dyn_into::<js_sys::Function>().ok());
        let started = set_timeout.is_some_and(|set_timeout| {
            set_timeout
                .call2(
                    &JsValue::UNDEFINED,
                    &resolve,
                    &JsValue::from_f64(duration.as_secs_f64() * 1000.0),
                )
                .is_ok()
        });
        if !started {
            // Without a timer, run at once rather than never.
            let _ = resolve.call0(&JsValue::UNDEFINED);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(timer).await;
}

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
