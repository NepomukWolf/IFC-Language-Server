//! Browser entrypoint: serves LSP over a pair of web streams, typically from a Web Worker.

use std::sync::Once;

use futures::stream::TryStreamExt;
use tower_lsp::{LspService, Server};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_streams::{ReadableStream, WritableStream};

use crate::backend::Backend;

/// Serves LSP until `input` closes. Both streams carry `Uint8Array` chunks of LSP base-protocol
/// messages (`Content-Length` header, blank line, JSON-RPC body), as on stdio.
#[wasm_bindgen]
pub async fn serve(input: web_sys::ReadableStream, output: web_sys::WritableStream) {
    console_error_panic_hook::set_once();
    tree_sitter_allocator::install();
    let input = ReadableStream::from_raw(input.unchecked_into())
        .into_stream()
        .map_ok(|chunk| chunk.unchecked_into::<js_sys::Uint8Array>().to_vec())
        .map_err(|error| std::io::Error::other(format!("{error:?}")))
        .into_async_read();
    let output = WritableStream::from_raw(output.unchecked_into()).into_async_write();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(input, output, socket).serve(service).await;
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
