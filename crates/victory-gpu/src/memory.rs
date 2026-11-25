//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`victory_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object).
//!
//! The copies are the ones every other access uses ([`crate::shadow`]: a
//! title buffer's copy, or the 4 MiB window of memory outside buffers), so
//! a root view and a raw address reach the same host bytes. This module
//! keeps what a shader needs to find them:
//!
//! - the **directory**: per 64 KiB page of title address space the number
//!   of the copy that holds the page (0: none) and the page's byte offset
//!   in it; immutable versions shared by recordings that see the same
//!   mappings. A later discovery never changes an already bound version;
//! - per view heap a run of slots with a raw unordered access view of each
//!   numbered copy (slot 0 of the run: a null view, for pages without a
//!   copy);
//! - the **faults**: the page numbers shaders found empty, read back here
//!   once a frame, so the copies exist from the next frame on. A pointer a
//!   shader takes out of memory cannot be known before it runs; the
//!   addresses a dispatch is given as root views are made ready before it.
//!
//! **A page that is not there when a shader touches it is a lost access**:
//! the read gives 0, the store is dropped, and the fault note only helps
//! the next frame. For the structure builder that is the same as a dropped
//! stage, so before a dispatch the host makes ready everything it can see
//! the dispatch name: the whole copy around each root view, around each
//! pair of root constants that is a title address and around each root
//! constant that is one in 256-byte units (the builder passes structures
//! all three ways), and the 4 MiB after each (a structure may cross a
//! window's end).
//! shortcut: what a stage reaches only through pointers it reads out of
//! memory (InitHeaders' table of structure addresses, LoadInstances'
//! instance records) is not made ready unless something else named it;
//! the fault count in the log says whether that happens. The whole answer
//! is the extent of the title's allocation around an address, which the
//! memory module of the layer knows and this one does not ask yet.
//!
//! shortcut: a copy that is numbered is kept for the rest of the run, and
//! there are at most [`COPIES`] of them (4 MiB windows mostly: 4 GiB at
//! most). Every dispatch of such a shader brings all numbered copies up to
//! date and, if it can store, declares all of them written (the bytes are
//! compared on the CPU afterwards, as for any unordered access view): a
//! dirty-page mask written by the shader would bound both.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use victory_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// Copies that can be numbered (the run of heap slots is one longer). A
/// listed frame of the opening cutscene names about 50 windows.
const COPIES: u32 = 1023;
/// A window of memory outside buffers (`shadow.rs`).
const CELL: u64 = 4 << 20;
/// A directory page.
const PAGE: u64 = 1 << 16;

struct Memory {
    /// Authoritative page mappings and their latest GPU-visible version.
    pages: HashMap<u64, [u32; 2]>,
    directory: Option<(ID3D12Resource, u64)>,
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    copies: Vec<Arc<Shadow>>,
    /// The frame the faults were last read in.
    frame: u64,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
}

// SAFETY: the mappings are of buffers the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// What a draw binds.
pub(crate) struct Ready {
    pub(crate) directory: u64,
    pub(crate) faults: u64,
    /// The view heap slot of the null view; copy `n`'s view is `n` further.
    pub(crate) base: u32,
}

impl Memory {
    fn new() -> Option<Memory> {
        let host = host()?;
        let make = |dwords: u64, what: &str| {
            let (buffer, data) = host.shared_buffer(dwords * 4)?;
            crate::host::name(&buffer, || format!("title memory by address: {what}"));
            // SAFETY: a fresh mapping of at least that many bytes.
            unsafe { std::ptr::write_bytes(data, 0, (dwords * 4) as usize) };
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some((buffer, data.cast::<u32>(), gpu))
        };
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        Some(Memory {
            pages: HashMap::new(),
            directory: None,
            faults: make(u64::from(MEMORY_FAULTS), "faults")?,
            copies: Vec::new(),
            frame: u64::MAX,
            refused: Default::default(),
        })
    }

    /// Gives the page of `address` its copy, if it has none and the memory
    /// is the title's.
    fn map(&mut self, address: u64) {
        let page = address / PAGE;
        // The last entry stays empty: addresses past the directory read it.
        if page >= u64::from(MEMORY_PAGES) - 1 {
            return;
        }
        if self.pages.contains_key(&page) {
            return;
        }
        let start = page * PAGE;
        // A shader's pointer, or a constant taken for an address, may be
        // anything: only memory the title has.
        if self.refused.contains(&page) {
            return;
        }
        if !crate::texture::committed(start as *const u8, PAGE as usize) {
            self.refused.insert(page);
            return;
        }
        // The copy a root view at that address would get; a buffer that
        // ends inside the page is passed over for what holds all of it.
        let whole = |s: &Arc<Shadow>| s.offset_in(start).is_some_and(|_| s.left(start) >= PAGE);
        let Some(shadow) = crate::shadow::find(start, 1)
            .filter(whole)
            .or_else(|| crate::shadow::find(start, PAGE).filter(whole))
        else {
            return;
        };
        // SAFETY: a live buffer.
        let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
        if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            return;
        }
        let n = match self.copies.iter().position(|c| Arc::ptr_eq(c, &shadow)) {
            Some(n) => n,
            None if self.copies.len() < COPIES as usize => {
                self.copies.push(shadow.clone());
                let n = self.copies.len() - 1;
                // Every page the copy holds whole, not only the one asked
                // for: a structure is more than 64 KiB.
                // shortcut: a page of a title buffer inside this one that
                // has a copy of its own is given this copy too.
                let (first, len) = shadow.span();
                let mut at = first.next_multiple_of(PAGE);
                while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
                    self.pages
                        .entry(at / PAGE)
                        .or_insert([n as u32 + 1, (at - first) as u32]);
                    at += PAGE;
                }
                n
            }
            None => {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::warn!(COPIES, "title memory by address: no number left for another copy; its pages read zeros")
                });
                return;
            }
        };
        let offset = shadow.offset_in(start).unwrap_or(0);
        self.pages.insert(page, [n as u32 + 1, offset as u32]);
        self.directory = None;
    }

    /// Creates a new version only after discovery changes the directory.
    /// Once bound by a recording, the resource is never written again.
    fn directory(&mut self) -> Option<&(ID3D12Resource, u64)> {
        if self.directory.is_none() {
            let size = u64::from(MEMORY_PAGES) * 8;
            let (buffer, data) = host()?.shared_buffer(size)?;
            crate::host::name(&buffer, || "title memory by address: directory".to_owned());
            // SAFETY: the fresh mapping is aligned and covers all directory entries.
            // The GPU cannot see the resource until its initialization is complete.
            let gpu = unsafe {
                std::ptr::write_bytes(data, 0, size as usize);
                let entries = data.cast::<[u32; 2]>();
                for (&page, &entry) in &self.pages {
                    entries.add(page as usize).write(entry);
                }
                buffer.GetGPUVirtualAddress()
            };
            self.directory = Some((buffer, gpu));
        }
        self.directory.as_ref()
    }

    /// The pages shaders found empty since the last frame.
    fn take_faults(&mut self) {
        let mut pages = Vec::new();
        for i in 1..MEMORY_FAULTS as usize {
            // SAFETY: inside the faults' mapping; the GPU may be writing,
            // a number missed now is written again next frame.
            let at = unsafe { self.faults.1.add(i) };
            // SAFETY: as above.
            let page = unsafe { at.read_volatile() };
            if page != 0 {
                pages.push(page);
                // SAFETY: as above.
                unsafe { at.write_volatile(0) };
            }
        }
        if !pages.is_empty() {
            // Each is an access that read 0 or was dropped.
            static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 50 {
                tracing::warn!(
                    pages = pages.len(),
                    first = format_args!("{:#x}", u64::from(pages[0]) * PAGE),
                    "title memory by address: shaders touched pages that had no copy (those accesses read 0 or were dropped); the copies are made now"
                );
            }
        }
        for page in pages {
            self.map(u64::from(page) * PAGE);
        }
    }

    /// What a dispatch names: the copy around `address` and the 4 MiB
    /// after it.
    fn name(&mut self, address: u64) {
        self.map(address);
        self.map((address & !(CELL - 1)) + CELL);
    }
}

/// Before a draw whose shader reads title memory by address: what its
/// root arguments name gets its copies (`addresses`: its root views;
/// `constants`: its root constants, see the module), every numbered copy
/// has its view in `heap`'s run and is brought up to date when `list` is
/// submitted (and, with `writes`, copied back after). None if the host
/// cannot hold it; the draw is then dropped.
pub(crate) fn ready(
    list: &HostList,
    heap: &HostHeap,
    addresses: &[u64],
    constants: &[u32],
    writes: bool,
) -> Option<Ready> {
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    let frame = crate::frame::current();
    if m.frame != frame {
        m.frame = frame;
        m.refused.clear();
        m.take_faults();
    }
    for &address in addresses {
        m.name(address);
    }
    // A title address in two constants (low dword, then bits 32 to 39), or
    // in one in 256-byte units. A constant that only looks like one names
    // memory nothing reads; one that is not the title's memory is refused.
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && (1..=0xFF).contains(&high)
            && c >= 0x1_0000
        {
            m.name(u64::from(c) | u64::from(high) << 32);
        }
        if c >= 0x0100_0000 {
            m.name(u64::from(c) << 8);
        }
    }
    let (directory, gpu) = m.directory()?.clone();
    let mut run = heap.memory_run.lock().unwrap_or_else(|e| e.into_inner());
    if run.is_none() {
        let base = heap.dynamic(COPIES + 1)?;
        heap.null_raw_uav_at(base);
        *run = Some((base, 0));
    }
    let (base, made) = run.as_mut()?;
    for (n, shadow) in m.copies.iter().enumerate().skip(*made) {
        let (_, len) = shadow.span();
        heap.raw_uav_at(*base + 1 + n as u32, &shadow.buffer, 0, len);
    }
    *made = m.copies.len();
    let base = *base;
    drop(run);

    let (read_from, write_from) =
        list.cover_title_memory(&directory, &m.faults.0, m.copies.len(), writes);
    for (n, shadow) in m.copies.iter().enumerate().skip(read_from.min(write_from)) {
        let (start, len) = shadow.span();
        if n >= read_from {
            shadow.refresh(list, start, len);
        }
        if n >= write_from {
            shadow.gpu_writes(list, start, len);
        }
    }
    Some(Ready {
        directory: gpu,
        faults: m.faults.2,
        base,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    #[test]
    fn closed_lists_hold_the_directory_version_they_recorded() {
        let host = host().expect("WARP is required for this probe");
        let candidate = (0..16)
            .map(|slot| 0x4_2000_0000 + slot * CELL)
            .find_map(|address| {
                // SAFETY: ask for our own fresh page inside the directory's range.
                let at = unsafe { VirtualAlloc(address as *const _, PAGE as usize, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
                (!at.is_null()).then_some(at)
            })
            .expect("allocate a synthetic title page");
        let address = candidate as u64;
        assert!(address / PAGE < u64::from(MEMORY_PAGES) - 1);
        crate::shadow::register(1, address, PAGE);
        let heap_a = HostHeap::new(false, 1).unwrap();
        let heap_b = HostHeap::new(false, 1).unwrap();
        let copy_directory = |list: &HostList, binding: Ready| {
            let target = host.read_back_buffer(8).unwrap();
            let guard = MEMORY.lock().unwrap();
            let (directory, gpu) = guard.as_ref().unwrap().directory.as_ref().unwrap();
            assert_eq!(binding.directory, *gpu);
            // SAFETY: open list; copy the two directory words using valid resources.
            unsafe {
                list.list.CopyBufferRegion(&target, 0, directory, address / PAGE * 8, 8)
            };
            target
        };
        let record = || {
            let list = host.new_list().unwrap();
            let binding = ready(&list, &heap_a, &[], &[], true).unwrap();
            let target = copy_directory(&list, binding);
            list.close();
            (list, target)
        };
        let read = |target: &ID3D12Resource| {
            let mut data = std::ptr::null_mut();
            // SAFETY: after the list's wait, read both copied words and unmap.
            unsafe {
                target.Map(0, Some(&D3D12_RANGE { Begin: 0, End: 8 }), Some(&mut data)).unwrap();
                let words = data.cast::<[u32; 2]>().read();
                target.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
                words
            }
        };
        let (control, control_target) = record();
        let (different_heap, different_target) = record();
        let (same_heap, same_target) = record();
        host.execute(&[&control]);
        assert_eq!(read(&control_target), [0, 0]);

        let continuing = host.new_list().unwrap();
        let old_binding = ready(&continuing, &heap_a, &[], &[], true).unwrap();
        let old_target = copy_directory(&continuing, old_binding);

        let discover = host.new_list().unwrap();
        let new_binding = ready(&discover, &heap_b, &[address], &[], true).unwrap();
        discover.close();
        assert_eq!(discover.test_shadow_steps(address as usize), (true, true));
        assert_eq!(heap_a.memory_run.lock().unwrap().unwrap().1, 0);
        assert_eq!(*heap_b.memory_run.lock().unwrap(), Some((new_binding.base, 1)));

        host.execute(&[&different_heap]);
        assert_eq!(read(&different_target), [0, 0]);
        assert_eq!(different_heap.test_shadow_steps(address as usize), (false, false));
        eprintln!("Closed list on a separate heap retained [0, 0] with zero installed copies");

        // This list had already called ready before another list discovered
        // the copy. It must cover that copy when it binds the new version.
        let binding = ready(&continuing, &heap_a, &[], &[], false).unwrap();
        assert_eq!(binding.directory, new_binding.directory);
        assert_eq!(continuing.test_shadow_steps(address as usize), (true, false));
        let binding = ready(&continuing, &heap_a, &[], &[], true).unwrap();
        let new_target = copy_directory(&continuing, binding);
        continuing.close();
        assert_eq!(continuing.test_shadow_steps(address as usize), (true, true));
        assert_eq!(heap_a.memory_run.lock().unwrap().unwrap().1, 1);

        host.execute(&[&same_heap]);
        assert_eq!(read(&same_target), [0, 0]);
        assert_eq!(same_heap.test_shadow_steps(address as usize), (false, false));
        eprintln!("Closed list on the shared heap retained [0, 0] after copy 1 was installed");

        host.execute(&[&continuing]);
        assert_eq!(read(&old_target), [0, 0]);
        assert_eq!(read(&new_target), [1, 0]);
        eprintln!("One recording read both versions and declared the new copy for refresh and write-back");

        drop((control, different_heap, same_heap, discover, continuing));
        MEMORY.lock().unwrap().take();
        crate::shadow::unregister(1, address);
        // SAFETY: release only the probe's private allocation.
        assert_ne!(unsafe { VirtualFree(candidate, 0, MEM_RELEASE) }, 0);
    }
}
