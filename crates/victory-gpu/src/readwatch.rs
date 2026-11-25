//! `VICTORY_READ_WATCH=1` (diagnostics builds): which of the title's own code
//! reads title memory of textures the GPU writes. The host keeps what shaders
//! write to a texture (unordered access; `=2`: render targets too) in its host
//! copy and never copies it back, so title code that reads such a texture on
//! the CPU reads bytes nothing wrote. Each such texture's pages are guarded
//! (`PAGE_GUARD`, which the system clears at the first touch); a touch from the
//! title's code (an address inside one of its images) is logged once per
//! texture and code address, touches from anything else are let through. The
//! guards are put back once a second. Looking only: nothing is copied back.
//!
//! shortcut: a read through another mapping of the same pages is not seen;
//! system calls that read guarded pages (the kernel reads of texture
//! refreshes) may fail while a guard stands.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::object::This;

/// 0 off, 1 textures written through unordered access, 2 also render targets.
pub(crate) fn level() -> u8 {
    static L: OnceLock<u8> = OnceLock::new();
    *L.get_or_init(|| {
        std::env::var("VICTORY_READ_WATCH").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0)
    })
}

const MOST: usize = 4096;
/// The watched ranges (start, end), appended under `NAMES`'s lock and read
/// without one by the exception handler.
static STARTS: [AtomicUsize; MOST] = [const { AtomicUsize::new(0) }; MOST];
static ENDS: [AtomicUsize; MOST] = [const { AtomicUsize::new(0) }; MOST];
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// What each watched range is (same index).
static NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Hits from the title's code not logged yet: (range index, code address,
/// address read).
static HITS: Mutex<Vec<(usize, usize, usize)>> = Mutex::new(Vec::new());
/// The title's images: (start, end, file name).
static TITLE: OnceLock<Vec<(usize, usize, String)>> = OnceLock::new();

/// A draw or dispatch about to be recorded writes title texture `resource`
/// (`how`: "unordered" or "target").
pub(crate) fn gpu_writes(resource: This, how: &'static str) {
    let level = level();
    if level == 0 || (how == "target" && level < 2) {
        return;
    }
    let Some((at, len, desc)) = crate::resource::title_span(resource) else {
        return;
    };
    if at == 0 || len == 0 {
        return;
    }
    static KNOWN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);
    if !KNOWN.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(at) {
        return;
    }
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    let n = COUNT.load(Ordering::Acquire);
    if n >= MOST {
        return;
    }
    title_images();
    install();
    STARTS[n].store(at, Ordering::Relaxed);
    ENDS[n].store(at + len, Ordering::Relaxed);
    names.push(format!("{how} {desc} at {at:#x}+{len:#x}"));
    COUNT.store(n + 1, Ordering::Release);
    drop(names);
    guard(at, len);
}

/// Puts `PAGE_GUARD` on the committed, accessible pages of `[at, at + len)`.
fn guard(at: usize, len: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualProtect, VirtualQuery,
    };
    let end = at + len;
    let mut p = at & !0xfff;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) } == 0 {
            return;
        }
        let region_end = (info.BaseAddress as usize + info.RegionSize).min((end + 0xfff) & !0xfff);
        // (no guard with no access, uncached or write-combined pages)
        const NOT_WITH_GUARD: u32 = PAGE_GUARD | PAGE_NOACCESS | 0x200 | 0x400;
        if info.State == MEM_COMMIT && info.Protect & NOT_WITH_GUARD == 0 && info.Protect != 0 {
            let mut old = 0;
            // SAFETY: committed pages of the title's; the protection keeps
            // their access and adds the one-shot guard.
            unsafe { VirtualProtect(p as *const _, region_end - p, info.Protect | PAGE_GUARD, &mut old) };
        }
        p = region_end.max(p + 0x1000);
    }
}

/// The title's images, found once: modules loaded from the image cache.
fn title_images() -> &'static [(usize, usize, String)] {
    TITLE.get_or_init(|| {
        use windows_sys::Win32::System::ProcessStatus::{
            K32EnumProcessModules, K32GetModuleFileNameExW, K32GetModuleInformation, MODULEINFO,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut found = Vec::new();
        // SAFETY: plain queries into buffers of ours.
        unsafe {
            let process = GetCurrentProcess();
            let mut modules = vec![std::ptr::null_mut(); 1024];
            let mut needed = 0;
            if K32EnumProcessModules(process, modules.as_mut_ptr(), (modules.len() * size_of::<usize>()) as u32, &mut needed) == 0 {
                return found;
            }
            for &m in &modules[..(needed as usize / size_of::<usize>()).min(modules.len())] {
                let mut name = [0u16; 512];
                let n = K32GetModuleFileNameExW(process, m, name.as_mut_ptr(), name.len() as u32) as usize;
                let name = String::from_utf16_lossy(&name[..n]);
                if !name.to_ascii_lowercase().contains("victory-image-") {
                    continue;
                }
                let mut info: MODULEINFO = std::mem::zeroed();
                if K32GetModuleInformation(process, m, &mut info, size_of::<MODULEINFO>() as u32) != 0 {
                    let start = info.lpBaseOfDll as usize;
                    found.push((start, start + info.SizeOfImage as usize, name.rsplit('\\').next().unwrap_or("").to_owned()));
                }
            }
        }
        tracing::info!(images = found.len(), "read watch: guarding title memory of textures the GPU writes (VICTORY_READ_WATCH)");
        found
    })
}

/// The handler (first in line) and the thread that logs hits and puts the
/// guards back, once.
fn install() {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
    };
    const STATUS_GUARD_PAGE_VIOLATION: i32 = 0x8000_0001_u32 as i32;
    unsafe extern "system" fn on_guard(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let (r, context) = unsafe { (&*(*info).ExceptionRecord, &*(*info).ContextRecord) };
        if r.ExceptionCode != STATUS_GUARD_PAGE_VIOLATION || r.NumberParameters < 2 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let at = r.ExceptionInformation[1];
        let n = COUNT.load(Ordering::Acquire);
        let Some(i) = (0..n).find(|&i| {
            let start = STARTS[i].load(Ordering::Relaxed);
            let end = ENDS[i].load(Ordering::Relaxed);
            at >= (start & !0xfff) && at < ((end + 0xfff) & !0xfff)
        }) else {
            return EXCEPTION_CONTINUE_SEARCH;
        };
        let rip = context.Rip as usize;
        let title = TITLE.get().is_some_and(|t| t.iter().any(|&(s, e, _)| rip >= s && rip < e));
        if title && let Ok(mut hits) = HITS.try_lock() && hits.len() < 4096 {
            hits.push((i, rip, at));
        }
        // The system cleared the page's guard; the access goes on.
        EXCEPTION_CONTINUE_EXECUTION
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: the handler only reads our atomics and takes a try-lock.
        unsafe { AddVectoredExceptionHandler(1, Some(on_guard)) };
        std::thread::Builder::new()
            .name("victory-read-watch".into())
            .spawn(|| {
                let mut seen = std::collections::HashSet::new();
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let hits = std::mem::take(&mut *HITS.lock().unwrap_or_else(|e| e.into_inner()));
                    if !hits.is_empty() {
                        let names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
                        let images = title_images();
                        for (i, rip, at) in hits {
                            if !seen.insert((i, rip)) {
                                continue;
                            }
                            let code = images
                                .iter()
                                .find(|&&(s, e, _)| rip >= s && rip < e)
                                .map(|(s, _, name)| format!("{name}+{:#x}", rip - s))
                                .unwrap_or_else(|| format!("{rip:#x}"));
                            let start = STARTS[i].load(Ordering::Relaxed);
                            tracing::warn!(
                                code,
                                offset = format_args!("{:#x}", at.wrapping_sub(start)),
                                texture = names.get(i).map(String::as_str).unwrap_or("?"),
                                "read watch: the title's code reads a texture the GPU writes (its bytes in title memory are not what the GPU wrote)"
                            );
                        }
                    }
                    let n = COUNT.load(Ordering::Acquire);
                    for i in 0..n {
                        let start = STARTS[i].load(Ordering::Relaxed);
                        let end = ENDS[i].load(Ordering::Relaxed);
                        guard(start, end - start);
                    }
                }
            })
            .ok();
    });
}
