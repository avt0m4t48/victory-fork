//! Diagnostics: a sampling profiler of the whole process (`VICTORY_SAMPLE=1`,
//! diagnostics builds). Every [`PERIOD`] each thread is suspended for an
//! instant and the instruction it stands at is taken; every 20 s the hottest
//! functions are logged, over all threads and for the busiest threads, by
//! name where our symbols know it (module and offset elsewhere: the title's
//! code, the system's).
//!
//! Why: the frame's time is spread over the title's threads, the recording
//! threads in our list calls, the submitting thread and the relay, and
//! coarse timers left every guess a ten-minute run. Kernel sampling (WPR)
//! needs an elevated session; this does not.
//!
//! Nothing is allocated or locked while a thread is suspended: it could hold
//! the heap's lock.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, GetThreadContext, SYMBOL_INFOW, SymFromAddrW, SymInitializeW, SymSetOptions,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread,
    THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

/// Time between two rounds of samples.
const PERIOD: Duration = Duration::from_millis(2);

/// The sampler's thread id, so that it never suspends itself.
static ME: AtomicU32 = AtomicU32::new(0);

/// Starts the sampler if `VICTORY_SAMPLE=1` (once).
pub(crate) fn start() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if std::env::var_os("VICTORY_SAMPLE").is_none_or(|v| v != "1") {
        return;
    }
    ONCE.call_once(|| {
        let _ = std::thread::Builder::new().name("victory sampler".into()).spawn(run);
    });
}

/// CONTEXT_AMD64 | CONTEXT_CONTROL: the instruction and stack pointers.
const CONTEXT_CONTROL: u32 = 0x0010_0001;

fn run() {
    // SAFETY: plain calls.
    ME.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
    // SAFETY: our own process; symbols from the executable's folder (its
    // .pdb lies beside it) and the default search path.
    unsafe {
        // SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_ANYTHING: the
        // system's dbghelp takes our .pdb for a mismatch without the last.
        SymSetOptions(0x0000_0002 | 0x0000_0004 | 0x0000_0040);
        let mut dir = [0u16; 520];
        let n = windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW(std::ptr::null_mut(), dir.as_mut_ptr(), 520) as usize;
        let end = dir[..n].iter().rposition(|&c| c == 0x5c).unwrap_or(0);
        dir[end] = 0;
        SymInitializeW(GetCurrentProcess(), dir.as_ptr(), 1);
    }
    let mut threads: Vec<(u32, HANDLE)> = Vec::new();
    let mut listed = Instant::now() - Duration::from_secs(10);
    let mut samples: Vec<(u32, u64, [u64; 3])> = Vec::with_capacity(1 << 20);
    let (exe_lo, exe_hi) = exe_range();
    let mut stack = [0u64; 1024];
    let mut logged = Instant::now();
    let mut names: HashMap<u64, String> = HashMap::new();
    loop {
        if listed.elapsed() > Duration::from_secs(1) {
            listed = Instant::now();
            for (_, h) in threads.drain(..) {
                // SAFETY: a handle we opened.
                unsafe { CloseHandle(h) };
            }
            threads = list_threads();
        }
        let mut round: [(u32, u64, [u64; 3]); 256] = [(0, 0, [0; 3]); 256];
        let mut n = 0;
        for &(tid, h) in threads.iter().take(256) {
            // SAFETY: plain data, filled in by the call.
            let mut ctx: CONTEXT = unsafe { std::mem::zeroed() };
            ctx.ContextFlags = CONTEXT_CONTROL;
            // SAFETY: a thread of ours, suspended only for the context read;
            // nothing between suspend and resume allocates or locks.
            let mut read = 0usize;
            let rip = unsafe {
                if SuspendThread(h) == u32::MAX {
                    continue;
                }
                let ok = GetThreadContext(h, &mut ctx) != 0;
                if ok {
                    // The top of its stack, through the kernel (it may end
                    // before 8 KiB): no fault, no allocation.
                    windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
                        GetCurrentProcess(),
                        ctx.Rsp as *const _,
                        stack.as_mut_ptr().cast(),
                        size_of_val(&stack),
                        &mut read,
                    );
                }
                ResumeThread(h);
                if ok { ctx.Rip } else { 0 }
            };
            if rip != 0 {
                // The first return addresses into our own code on its stack
                // (a scan: what a thread waiting on a lock waits for).
                let mut callers = [0u64; 3];
                let mut k = 0;
                for &w in &stack[..read / 8] {
                    if k == 3 {
                        break;
                    }
                    if w > exe_lo && w < exe_hi && (k == 0 || callers[k - 1] != w) {
                        callers[k] = w;
                        k += 1;
                    }
                }
                round[n] = (tid, rip, callers);
                n += 1;
            }
        }
        if samples.len() + n <= samples.capacity() {
            samples.extend_from_slice(&round[..n]);
        }
        if logged.elapsed() > Duration::from_secs(20) {
            logged = Instant::now();
            report(&samples, &mut names);
            samples.clear();
        }
        std::thread::sleep(PERIOD);
    }
}

/// The executable's image: where our own code lies.
fn exe_range() -> (u64, u64) {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::ProcessStatus::{GetModuleInformation, MODULEINFO};
    // SAFETY: plain calls with valid out pointers.
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let mut info: MODULEINFO = std::mem::zeroed();
        if GetModuleInformation(GetCurrentProcess(), module, &mut info, size_of::<MODULEINFO>() as u32) == 0 {
            return (0, 0);
        }
        let lo = info.lpBaseOfDll as u64;
        (lo, lo + u64::from(info.SizeOfImage))
    }
}

/// Every thread of the process but the sampler, opened for sampling.
fn list_threads() -> Vec<(u32, HANDLE)> {
    let mut out = Vec::new();
    // SAFETY: plain calls; the snapshot is closed below.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap.is_null() || snap as isize == -1 {
            return out;
        }
        let mut e: THREADENTRY32 = std::mem::zeroed();
        e.dwSize = size_of::<THREADENTRY32>() as u32;
        let pid = GetCurrentProcessId();
        let me = ME.load(Ordering::Relaxed);
        let mut more = Thread32First(snap, &mut e) != 0;
        while more {
            if e.th32OwnerProcessID == pid && e.th32ThreadID != me {
                let h = OpenThread(
                    THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                    0,
                    e.th32ThreadID,
                );
                if !h.is_null() {
                    out.push((e.th32ThreadID, h));
                }
            }
            more = Thread32Next(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    out
}

/// The function at `rip` by name, or its module and offset.
fn name_of(rip: u64, names: &mut HashMap<u64, String>) -> String {
    if let Some(n) = names.get(&rip) {
        return n.clone();
    }
    #[repr(C)]
    struct Sym {
        info: SYMBOL_INFOW,
        rest: [u16; 512],
    }
    // SAFETY: plain data, filled in by the call.
    let mut s: Sym = unsafe { std::mem::zeroed() };
    s.info.SizeOfStruct = size_of::<SYMBOL_INFOW>() as u32;
    s.info.MaxNameLen = 512;
    let mut displacement = 0u64;
    // SAFETY: our own process and a buffer of the declared size.
    let named = unsafe { SymFromAddrW(GetCurrentProcess(), rip, &mut displacement, &mut s.info) } != 0;
    let name = if named {
        let len = (s.info.NameLen as usize).min(512);
        // SAFETY: the name runs on from `Name` into `rest`, `len` units.
        let units = unsafe { std::slice::from_raw_parts(s.info.Name.as_ptr(), len) };
        let mut name = String::from_utf16_lossy(units);
        // Generic and closure suffixes make every instance its own row.
        if let Some(at) = name.find("::{{closure}}") {
            name.truncate(at + 13);
        }
        name
    } else {
        module_of(rip)
    };
    names.insert(rip, name.clone());
    name
}

/// "module+0xoffset" for an address no symbol names.
fn module_of(rip: u64) -> String {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW,
        GetModuleHandleExW,
    };
    let mut module = std::ptr::null_mut();
    // SAFETY: plain calls with valid out pointers.
    unsafe {
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            rip as *const u16,
            &mut module,
        ) == 0
        {
            return "(no module: title code or generated)".to_owned();
        }
        let mut path = [0u16; 260];
        let n = GetModuleFileNameW(module, path.as_mut_ptr(), 260) as usize;
        let path = String::from_utf16_lossy(&path[..n]);
        let file = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_owned();
        // Grouped by module and 4 KiB of code: one row per hot stretch.
        format!("{file}+{:#x}", (rip - module as u64) & !0xfff)
    }
}

fn report(samples: &[(u32, u64, [u64; 3])], names: &mut HashMap<u64, String>) {
    if samples.is_empty() {
        return;
    }
    let mut by_name: HashMap<String, u64> = HashMap::new();
    let mut by_thread: HashMap<u32, HashMap<String, u64>> = HashMap::new();
    let mut busy: HashMap<u32, u64> = HashMap::new();
    let mut chains: HashMap<u32, HashMap<String, u64>> = HashMap::new();
    for &(tid, rip, callers) in samples {
        let name = name_of(rip, names);
        // The function, then up to two callers of ours: short names.
        let mut chain = short(&name);
        for &c in callers.iter().take_while(|&&c| c != 0).take(2) {
            chain.push_str(" < ");
            chain.push_str(&short(&name_of(c, names)));
        }
        *chains.entry(tid).or_default().entry(chain).or_default() += 1;
        // A thread standing in a wait is idle, not busy.
        let waiting = name.contains("NtWaitFor")
            || name.contains("NtDelayExecution")
            || name.contains("ZwWaitFor")
            || name.contains("NtRemoveIoCompletion")
            || name.contains("NtWaitForAlertByThreadId")
            || name.contains("NtSignalAndWaitForSingleObject");
        *by_name.entry(name.clone()).or_default() += 1;
        *by_thread.entry(tid).or_default().entry(name).or_default() += 1;
        if !waiting {
            *busy.entry(tid).or_default() += 1;
        }
    }
    let total = samples.len() as f64;
    let mut rows: Vec<(String, u64)> = by_name.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    let line: Vec<String> = rows
        .iter()
        .take(40)
        .map(|(n, c)| format!("{:.1}% {n}", *c as f64 * 100.0 / total))
        .collect();
    tracing::info!("sampler, all threads ({} samples): {}", samples.len(), line.join(" | "));
    let mut threads: Vec<(u32, u64)> = busy.into_iter().collect();
    threads.sort_by(|a, b| b.1.cmp(&a.1));
    for (tid, b) in threads.into_iter().take(10) {
        let Some(map) = chains.get(&tid) else { continue };
        let all: u64 = map.values().sum();
        let mut rows: Vec<(&String, &u64)> = map.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1));
        let line: Vec<String> = rows
            .iter()
            .take(14)
            .map(|(n, c)| format!("{:.0}% {n}", **c as f64 * 100.0 / all as f64))
            .collect();
        tracing::info!(
            "sampler, thread {tid}: busy in {:.0}% of its samples; {}",
            b as f64 * 100.0 / all.max(1) as f64,
            line.join(" | ")
        );
    }
}

/// A symbol's last two path segments (`module::function`), without the
/// crate path and generic arguments.
fn short(name: &str) -> String {
    let base = match name.find('<') {
        Some(at) if at > 0 => &name[..at],
        _ => name,
    };
    let parts: Vec<&str> = base.rsplit("::").take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("::")
}
