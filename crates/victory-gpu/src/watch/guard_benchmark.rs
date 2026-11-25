use super::{BLOCK, PAGE, guard, title_memory_remapped, writing};
use std::hint::black_box;
use std::time::Instant;
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE,
    UnmapViewOfFile,
};

const LENGTH: usize = 174 << 20;

struct View(MEMORY_MAPPED_VIEW_ADDRESS);

impl Drop for View {
    fn drop(&mut self) {
        title_memory_remapped(self.0.Value as usize, LENGTH);
        // SAFETY: our view, still live.
        unsafe { UnmapViewOfFile(self.0) };
    }
}

#[test]
#[ignore = "synthetic timing comparison"]
fn native_guard_queries_over_a_174_mib_view() {
    // SAFETY: a fresh page-file mapping with a lifetime held by the view.
    let view = unsafe {
        let mapping = CreateFileMappingW(
            INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, LENGTH as u32, std::ptr::null(),
        );
        assert!(!mapping.is_null());
        let mapped = MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, LENGTH);
        CloseHandle(mapping);
        assert!(!mapped.Value.is_null());
        View(mapped)
    };
    let start = view.0.Value as usize;
    let end = start + LENGTH;
    let blocks = [7, 231, 1201, 2503];
    let expected: Vec<_> = blocks.iter().flat_map(|block| {
        let first = (start + block * BLOCK) / PAGE;
        first..first + BLOCK / PAGE
    }).collect();
    let dirty = || {
        for block in blocks {
            let address = start + block * BLOCK;
            writing(address, 1);
            // SAFETY: our view, opened for a write immediately above.
            unsafe { (address as *mut u8).write_volatile(1) };
        }
    };
    assert_eq!(guard(start, end).unwrap().len(), LENGTH / PAGE);
    for _ in 0..32 {
        assert!(guard(start, end).unwrap().is_empty());
        dirty();
        assert_eq!(guard(start, end).unwrap(), expected);
    }

    const SAMPLES: usize = 9;
    const CLEAN_CALLS: usize = 1000;
    const DIRTY_CALLS: usize = 250;
    let mut clean_ns = Vec::with_capacity(SAMPLES);
    let mut dirty_ns = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        for _ in 0..CLEAN_CALLS {
            assert!(black_box(guard(start, end)).unwrap().is_empty());
        }
        clean_ns.push(started.elapsed().as_nanos() / CLEAN_CALLS as u128);
        let mut elapsed_ns = 0;
        for _ in 0..DIRTY_CALLS {
            dirty();
            let started = Instant::now();
            let got = black_box(guard(start, end)).unwrap();
            elapsed_ns += started.elapsed().as_nanos();
            assert_eq!(got, expected);
        }
        dirty_ns.push(elapsed_ns / DIRTY_CALLS as u128);
    }
    clean_ns.sort_unstable();
    dirty_ns.sort_unstable();
    eprintln!("guard_ns clean median={} all={clean_ns:?}", clean_ns[SAMPLES / 2]);
    eprintln!("guard_ns four_dirty median={} all={dirty_ns:?}", dirty_ns[SAMPLES / 2]);
}
