#[allow(dead_code)]
#[path = "../benches/support/mod.rs"]
mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use tower_lsp::lsp_types::Range;

use support::{Model, Session};

struct CountingAllocator;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_alloc(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
            record_alloc(new_size);
        }
        new_pointer
    }
}

fn record_alloc(size: usize) {
    let current = CURRENT.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(current, Ordering::Relaxed);
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn mib(bytes: usize) -> String {
    format!("{bytes} B ({:.1} MiB)", bytes as f64 / (1024.0 * 1024.0))
}

fn report(label: &str, counter: &AtomicUsize) {
    println!(
        "{:<20}{}",
        format!("{label}:"),
        mib(counter.load(Ordering::Relaxed))
    );
}

fn main() {
    let path = std::env::args_os().nth(1).map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("samples/Duplex_A_20110907-IFC2x3.ifc"),
        PathBuf::from,
    );
    let model = Model::load(&path);
    let uri = model.uri();
    let mut session = Session::new();
    println!("model:              {}", model.name);
    println!("file size:          {}", mib(model.text.len()));
    report("heap before open", &CURRENT);

    let diagnostics = session.open(&uri, model.text.clone());
    println!("diagnostics:        {}", diagnostics.len());
    drop(diagnostics);
    report("peak after open", &PEAK);
    report("heap after open", &CURRENT);

    let offset = model.cartesian_point_digit();
    let range = Range::new(model.position(offset), model.position(offset + 1));
    let replacement = if &model.text[offset..offset + 1] == "9" {
        "8"
    } else {
        "9"
    };
    PEAK.store(CURRENT.load(Ordering::Relaxed), Ordering::Relaxed);
    drop(session.change(&uri, range, replacement));
    report("peak during edit", &PEAK);
    report("heap after edit", &CURRENT);
}
