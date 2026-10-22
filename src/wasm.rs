//! Browser entrypoint: serves LSP over a pair of web streams, typically from a Web Worker.

use std::cell::RefCell;
use std::io;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::Once;
use std::task::{Context, Poll, ready};

use futures::channel::oneshot;
use futures::future::{self, Either};
use futures::io::AsyncWrite;
use futures::{SinkExt, StreamExt, TryStreamExt};
use js_sys::Uint8Array;
use tower_lsp::{LspService, Server};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use wasm_streams::writable::IntoSink;
use wasm_streams::{ReadableStream, WritableStream};

use crate::backend::Backend;

/// Serves LSP until `input` closes. Both streams carry `Uint8Array` chunks of LSP base-protocol
/// messages (`Content-Length` header, blank line, JSON-RPC body), as on stdio.
///
/// Resolves once `input` closes and the server has finished. Rejects with the cause as soon as
/// either stream fails, dropping the server.
#[wasm_bindgen]
pub async fn serve(
    input: web_sys::ReadableStream,
    output: web_sys::WritableStream,
) -> Result<(), JsValue> {
    console_error_panic_hook::set_once();
    tree_sitter_allocator::install();
    // tower-lsp only logs transport errors, which the browser build compiles out, so the stream
    // adapters report them here instead of passing them on.
    let (failure, mut failed) = Failure::new();
    let input = ReadableStream::from_raw(input.unchecked_into())
        .into_stream()
        .scan(failure.clone(), |failure, chunk| {
            future::ready(match chunk {
                Ok(chunk) => Some(Ok::<_, io::Error>(
                    chunk.unchecked_into::<Uint8Array>().to_vec(),
                )),
                Err(cause) => {
                    failure.record(cause);
                    None
                }
            })
        })
        .into_async_read();
    let mut output_stream = WritableStream::from_raw(output.unchecked_into());
    let writer = output_stream.get_writer();
    // A write failure surfaces here even when the framing layer never polls that write again.
    let output_closed = JsFuture::from(writer.as_raw().closed());
    spawn_local({
        let failure = failure.clone();
        async move {
            if let Err(cause) = output_closed.await {
                failure.record(cause);
            }
        }
    });
    let output = Output {
        sink: writer.into_sink(),
        failure: failure.clone(),
    };
    let (service, socket) = LspService::new(Backend::new);
    let server = pin!(Server::new(input, output, socket).serve(service));
    match future::select(server, &mut failed).await {
        Either::Left(((), _)) => match failed.try_recv() {
            Ok(Some(cause)) => Err(cause),
            _ => Ok(()),
        },
        Either::Right((cause, _)) => Err(cause.expect("`serve` holds a sender")),
    }
}

/// The first transport failure, delivered once to `serve`.
#[derive(Clone)]
struct Failure(Rc<RefCell<Option<oneshot::Sender<JsValue>>>>);

impl Failure {
    fn new() -> (Self, oneshot::Receiver<JsValue>) {
        let (sender, receiver) = oneshot::channel();
        (Self(Rc::new(RefCell::new(Some(sender)))), receiver)
    }

    fn record(&self, cause: JsValue) {
        if let Some(sender) = self.0.borrow_mut().take() {
            // Fails only once `serve` has returned, when nobody is left to tell.
            let _ = sender.send(cause);
        }
    }
}

/// Writes to a `WritableStream`. A failed write is recorded and stays pending, since `serve`
/// drops the server on any failure.
struct Output<'stream> {
    sink: IntoSink<'stream>,
    failure: Failure,
}

impl Output<'_> {
    fn check(&self, result: Result<(), JsValue>) -> Poll<io::Result<()>> {
        match result {
            Ok(()) => Poll::Ready(Ok(())),
            Err(cause) => {
                self.failure.record(cause);
                Poll::Pending
            }
        }
    }
}

impl AsyncWrite for Output<'_> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let sent = ready!(this.sink.poll_ready_unpin(cx))
            .and_then(|()| this.sink.start_send_unpin(Uint8Array::from(buf).into()));
        this.check(sent).map_ok(|()| buf.len())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let flushed = ready!(this.sink.poll_flush_unpin(cx));
        this.check(flushed)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let closed = ready!(this.sink.poll_close_unpin(cx));
        this.check(closed)
    }
}

/// Routes tree-sitter's C allocations through Rust's global allocator.
///
/// wasm32-unknown-unknown has no libc, so `tree-sitter-language` compiles in a minimal bump
/// allocator for the C code. That allocator's heap begins at linear-memory address 0 and grows
/// upward over memory the Rust side already uses for its stack and data, and it cannot grow past
/// 4 MB. Small files happen to fit; a syntax tree of a few hundred instances overwrites the Rust
/// stack and the module fails in unrelated places. Giving tree-sitter Rust's allocator keeps both
/// languages on one heap.
mod tree_sitter_allocator {
    use std::alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc};
    use std::ffi::c_void;

    use super::Once;

    /// Each block starts with its size, padded to the alignment C expects from `malloc`.
    const HEADER: usize = 16;

    pub fn install() {
        static INSTALL: Once = Once::new();
        // SAFETY: runs before the first parser exists, so no block crosses allocators.
        INSTALL.call_once(|| unsafe {
            tree_sitter::set_allocator(Some(tree_sitter::Allocator {
                malloc: ts_malloc,
                calloc: ts_calloc,
                realloc: ts_realloc,
                free: ts_free,
            }));
        });
    }

    fn layout(size: usize) -> Option<Layout> {
        Layout::from_size_align(size.checked_add(HEADER)?, HEADER).ok()
    }

    unsafe fn finish(block: *mut u8, size: usize) -> *mut c_void {
        if block.is_null() {
            return std::ptr::null_mut();
        }
        unsafe {
            block.cast::<usize>().write(size);
            block.add(HEADER).cast()
        }
    }

    unsafe fn block_of(ptr: *mut c_void) -> (*mut u8, Layout) {
        unsafe {
            let block = ptr.cast::<u8>().sub(HEADER);
            let size = block.cast::<usize>().read();
            (
                block,
                Layout::from_size_align_unchecked(size + HEADER, HEADER),
            )
        }
    }

    unsafe extern "C" fn ts_malloc(size: usize) -> *mut c_void {
        let Some(layout) = layout(size) else {
            return std::ptr::null_mut();
        };
        unsafe { finish(alloc(layout), size) }
    }

    unsafe extern "C" fn ts_calloc(count: usize, size: usize) -> *mut c_void {
        let Some(layout) = count.checked_mul(size).and_then(layout) else {
            return std::ptr::null_mut();
        };
        unsafe { finish(alloc_zeroed(layout), count * size) }
    }

    unsafe extern "C" fn ts_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
        if ptr.is_null() {
            return unsafe { ts_malloc(size) };
        }
        if layout(size).is_none() {
            return std::ptr::null_mut();
        }
        unsafe {
            let (block, old) = block_of(ptr);
            finish(realloc(block, old, size + HEADER), size)
        }
    }

    unsafe extern "C" fn ts_free(ptr: *mut c_void) {
        if ptr.is_null() {
            return;
        }
        unsafe {
            let (block, layout) = block_of(ptr);
            dealloc(block, layout);
        }
    }
}
