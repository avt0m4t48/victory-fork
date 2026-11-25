//! What the CPU does with what shaders wrote (the probe,
//! `VICTORY_KEEP_PROBE=1`; off by default, looking only).
//!
//! The relay copies what shaders wrote back into title memory after every
//! flight (`crate::host` `relay_land`): pieces of copies in the card's
//! memory are fetched and merged (`Shadow::card_merge_relay`), copies in
//! the PC's memory are compared and merged (`Shadow::relay_merge`). Our own
//! write shows as a title write, so the next refresh copies the same bytes
//! into the copy again (`copy_prep`), and for a copy in the card's memory
//! sends them up again (`card_up`). Where the next to read them is a shader
//! again, all of that is a round trip for nothing.
//!
//! With the switch every 64 KiB block of title memory a merge wrote gets a
//! window: its pages are guarded (`PAGE_GUARD`, kept on top of whatever
//! protection `crate::watch` gives them), and the first touch of the CPU
//! (the title's code, any other code: ours, the runtime's) is counted and
//! lifts the guard. The window closes when a merge writes the block again
//! (the next GPU write that reaches title memory). A window closed with no
//! touch is a round trip the cut would save: the bytes fetched and merged,
//! and the bytes our refreshes then read from the block again (`echo`).
//!
//! Our own reads and writes of title memory through the kernel (which fail
//! on a guarded page) lift the guard for their length and put it back; a
//! touch of the title's in that moment is missed, so "untouched" is an
//! upper bound. A table every 20 s.
//!
//! shortcut: the relay's merges only (`VICTORY_RELAY`); the settle's own
//! copy-back without it opens no window. A system call the title makes
//! with a buffer in a guarded block (a file write from it) fails once.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

/// The unit watched: the console's page, the unit of the copies' pieces.
const BLOCK: usize = crate::watch::PAGE;

/// `VICTORY_KEEP_PROBE=1`.
pub(crate) fn probe() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_KEEP_PROBE").is_some_and(|v| v == "1");
        if on {
            tracing::info!(
                "keep probe: blocks of title memory the relay writes back are guarded until the CPU touches them \
                 or the GPU writes them again (VICTORY_KEEP_PROBE=1)"
            );
        }
        on
    })
}

/// One block's window, from a merge to the next.
struct Window {
    /// Of a copy in the card's memory.
    card: bool,
    /// Its guard stands (or stands again once `lifted` is 0).
    armed: bool,
    /// Our own reads and writes through the kernel under way that took the
    /// guard down.
    lifted: u32,
    /// Bit 0 the title's code touched it, bit 1 other code, bit 2 a
    /// kernel write for the title (a file read into it).
    touched: u8,
    /// Bytes the merge that opened it wrote.
    merged: u64,
    /// Bytes our refreshes read from it while it was untouched.
    echo: u64,
    /// The copy (start, bytes), for the table.
    copy: (u64, u64),
}

static PROBE: Mutex<BTreeMap<usize, Window>> = Mutex::new(BTreeMap::new());
/// Windows in `PROBE` (the hooks' fast way out).
static OPEN: AtomicUsize = AtomicUsize::new(0);

/// Since the last table: windows opened; closed untouched (card, PC),
/// touched by the title's code, by other code only, by a kernel write only;
/// dropped (remapped); bytes merged in windows closed untouched (card, PC);
/// echo bytes in them (card, PC); bytes merged in touched windows; touches
/// counted by the handler.
static STATS: [AtomicU64; 14] = [const { AtomicU64::new(0) }; 14];
const OPENED: usize = 0;
const CLEAN_CARD: usize = 1;
const CLEAN_PC: usize = 2;
const BY_TITLE: usize = 3;
const BY_OTHER: usize = 4;
const BY_IO: usize = 5;
const DROPPED: usize = 6;
const CLEAN_MERGED_CARD: usize = 7;
const CLEAN_MERGED_PC: usize = 8;
const CLEAN_ECHO_CARD: usize = 9;
const CLEAN_ECHO_PC: usize = 10;
const TOUCHED_MERGED: usize = 11;
const FAULTS: usize = 12;
const ECHO_ALL: usize = 13;

/// Per copy (start, bytes), since the start: windows closed untouched and
/// their merged bytes, windows touched by the title's code, by other code
/// or a kernel write.
static PER_COPY: Mutex<BTreeMap<(u64, u64), [u64; 4]>> = Mutex::new(BTreeMap::new());

fn probe_map() -> std::sync::MutexGuard<'static, BTreeMap<usize, Window>> {
    PROBE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Puts the guard on the committed pages of `block` that can take one;
/// whether any did.
fn arm(block: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualProtect, VirtualQuery,
    };
    let (mut p, end) = (block * BLOCK, (block + 1) * BLOCK);
    let mut any = false;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) } == 0 {
            break;
        }
        let to = (info.BaseAddress as usize).saturating_add(info.RegionSize).min(end);
        // (no guard with no access, uncached or write-combined pages)
        const NOT_WITH_GUARD: u32 = PAGE_GUARD | PAGE_NOACCESS | 0x200 | 0x400;
        if info.State == MEM_COMMIT && info.Protect & NOT_WITH_GUARD == 0 && info.Protect != 0 {
            let mut old = 0;
            // SAFETY: committed title pages; their access stays, the
            // one-shot guard is added.
            any |= unsafe { VirtualProtect(p as *const _, to - p, info.Protect | PAGE_GUARD, &mut old) } != 0;
        }
        p = to.max(p + 4096);
    }
    any
}

/// Takes the guard off `block`'s pages that still have it.
fn lift(block: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, VirtualProtect, VirtualQuery,
    };
    let (mut p, end) = (block * BLOCK, (block + 1) * BLOCK);
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        if unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) } == 0 {
            break;
        }
        let to = (info.BaseAddress as usize).saturating_add(info.RegionSize).min(end);
        if info.State == MEM_COMMIT && info.Protect & PAGE_GUARD != 0 {
            let mut old = 0;
            // SAFETY: committed title pages; only our guard comes off.
            unsafe { VirtualProtect(p as *const _, to - p, info.Protect & !PAGE_GUARD, &mut old) };
        }
        p = to.max(p + 4096);
    }
}

/// Counts a window that ends (a merge writes its block again, or the
/// memory is remapped: `dropped`).
fn close(w: &Window, dropped: bool) {
    let stat = |i: usize, n: u64| STATS[i].fetch_add(n, Relaxed);
    let mut per = PER_COPY.lock().unwrap_or_else(|e| e.into_inner());
    let tally = per.entry(w.copy).or_insert([0; 4]);
    if w.touched & 1 != 0 {
        stat(BY_TITLE, 1);
        stat(TOUCHED_MERGED, w.merged);
        tally[2] += 1;
    } else if w.touched != 0 {
        stat(if w.touched & 2 != 0 { BY_OTHER } else { BY_IO }, 1);
        stat(TOUCHED_MERGED, w.merged);
        tally[3] += 1;
    } else if dropped {
        stat(DROPPED, 1);
    } else {
        stat(if w.card { CLEAN_CARD } else { CLEAN_PC }, 1);
        stat(if w.card { CLEAN_MERGED_CARD } else { CLEAN_MERGED_PC }, w.merged);
        stat(if w.card { CLEAN_ECHO_CARD } else { CLEAN_ECHO_PC }, w.echo);
        tally[0] += 1;
        tally[1] += w.merged;
    }
}

/// A merge of the relay wrote what shaders wrote into title memory
/// `[at, at + n)`, of the copy `copy` (start, bytes): the windows of its
/// blocks close, and new ones open.
pub(crate) fn merged(at: usize, n: usize, card: bool, copy: (u64, u64)) {
    if !probe() || n == 0 {
        return;
    }
    install();
    crate::watch::guards_held(|| {
        let mut map = probe_map();
        for block in at / BLOCK..(at + n).div_ceil(BLOCK) {
            let bytes = ((block + 1) * BLOCK).min(at + n) - (block * BLOCK).max(at);
            let old = map.remove(&block);
            if let Some(old) = &old {
                OPEN.fetch_sub(1, Relaxed);
                close(old, false);
            }
            // A read of ours under way keeps the guard down until it is
            // done (`around` puts it back); else it goes on the pages that
            // lack it (those of a window still armed have it).
            let lifted = old.as_ref().map_or(0, |o| o.lifted);
            let still = old.as_ref().is_some_and(|o| o.armed);
            let armed = lifted > 0 || arm(block) || still;
            if armed {
                let w = Window { card, armed, lifted, touched: 0, merged: bytes as u64, echo: 0, copy };
                map.insert(block, w);
                OPEN.fetch_add(1, Relaxed);
                STATS[OPENED].fetch_add(1, Relaxed);
            }
        }
    });
}

/// Our refresh reads title memory `[at, at + len)` into a copy: bytes of
/// untouched windows read again (the echo of our own merge).
pub(crate) fn echo(at: usize, len: usize) {
    if OPEN.load(Relaxed) == 0 || len == 0 {
        return;
    }
    let mut map = probe_map();
    for (&block, w) in map.range_mut(at / BLOCK..(at + len).div_ceil(BLOCK)) {
        if w.touched == 0 {
            let bytes = ((block + 1) * BLOCK).min(at + len) - (block * BLOCK).max(at);
            w.echo += bytes as u64;
            STATS[ECHO_ALL].fetch_add(bytes as u64, Relaxed);
        }
    }
}

/// Runs `f`, a read or write of ours through the kernel over title memory
/// `[a, a + an)` and `[b, b + bn)`, with the guards of windows there taken
/// down meanwhile (the kernel's copy fails on a guarded page).
pub(crate) fn around<R>(a: usize, an: usize, b: usize, bn: usize, f: impl FnOnce() -> R) -> R {
    if OPEN.load(Relaxed) == 0 {
        return f();
    }
    let blocks = |map: &BTreeMap<usize, Window>| -> Vec<usize> {
        let mut found: Vec<usize> = [(a, an), (b, bn)]
            .iter()
            .filter(|r| r.1 > 0)
            .flat_map(|&(at, n)| map.range(at / BLOCK..(at + n).div_ceil(BLOCK)).map(|(&k, _)| k).collect::<Vec<_>>())
            .collect();
        found.sort_unstable();
        found.dedup();
        found
    };
    if blocks(&probe_map()).is_empty() {
        return f();
    }
    let taken = crate::watch::guards_held(|| {
        let mut map = probe_map();
        let found = blocks(&map);
        for &block in &found {
            if let Some(w) = map.get_mut(&block) {
                if w.armed && w.lifted == 0 {
                    lift(block);
                }
                w.lifted += 1;
            }
        }
        found
    });
    let out = f();
    crate::watch::guards_held(|| {
        let mut map = probe_map();
        for block in taken {
            let Some(w) = map.get_mut(&block) else {
                continue;
            };
            w.lifted = w.lifted.saturating_sub(1);
            if w.lifted == 0 && w.armed {
                arm(block);
            }
        }
    });
    out
}

/// The runtime is about to write title memory `[at, at + len)` through the
/// kernel for the title (a file or DirectStorage read): a touch of the
/// title's, and the guards come off (the kernel's write would fail).
pub(crate) fn io_write(at: usize, len: usize) {
    if OPEN.load(Relaxed) == 0 || len == 0 {
        return;
    }
    let end = at.saturating_add(len);
    crate::watch::guards_held(|| {
        let mut map = probe_map();
        for (&block, w) in map.range_mut(at / BLOCK..end.div_ceil(BLOCK)) {
            w.touched |= 4;
            if w.armed {
                w.armed = false;
                if w.lifted == 0 {
                    lift(block);
                }
            }
        }
    });
}

/// `crate::watch` changed the protection of blocks `[from, to)` (it holds
/// its lock): the guards of armed windows there are put back on top.
pub(crate) fn reprotected(from: usize, to: usize) {
    if OPEN.load(Relaxed) == 0 {
        return;
    }
    let map = probe_map();
    for (&block, w) in map.range(from..to) {
        if w.armed && w.lifted == 0 {
            arm(block);
        }
    }
}

/// Title memory `[at, at + len)` is remapped (`crate::watch`, which holds
/// its lock): what it shows is new, its windows end.
pub(crate) fn remapped(at: usize, len: usize) {
    if OPEN.load(Relaxed) == 0 || len == 0 {
        return;
    }
    let mut map = probe_map();
    let blocks: Vec<usize> = map.range(at / BLOCK..at.saturating_add(len).div_ceil(BLOCK)).map(|(&k, _)| k).collect();
    for block in blocks {
        if let Some(w) = map.remove(&block) {
            OPEN.fetch_sub(1, Relaxed);
            close(&w, true);
            // No guard of ours may stay where no window is: its fault
            // would find no one to take it. (Harmless where the memory is
            // going or new.)
            lift(block);
        }
    }
}

/// The handler that counts the first touch of a guarded block, once.
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
        let block = r.ExceptionInformation[1] / BLOCK;
        let title = title_code(context.Rip as usize);
        // The same lock order as everywhere: the guards', then ours. No
        // code holding either touches title memory.
        crate::watch::guards_held(|| {
            let mut map = probe_map();
            let Some(w) = map.get_mut(&block) else {
                return EXCEPTION_CONTINUE_SEARCH;
            };
            // The system took the guard off the page touched; the block's
            // other pages lose theirs too: one touch is enough to know.
            if w.armed {
                w.armed = false;
                if w.lifted == 0 {
                    lift(block);
                }
            }
            w.touched |= if title { 1 } else { 2 };
            STATS[FAULTS].fetch_add(1, Relaxed);
            EXCEPTION_CONTINUE_EXECUTION
        })
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Before the first fault: finding the title's images takes locks.
        let _ = title_code(0);
        // SAFETY: the handler touches our state, the guards' and
        // protections of blocks we guarded.
        unsafe { AddVectoredExceptionHandler(1, Some(on_guard)) };
    });
}

/// Whether `rip` is in the title's own code.
fn title_code(rip: usize) -> bool {
    #[cfg(feature = "diagnostics")]
    return crate::readwatch::title_code(rip);
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = rip;
        false
    }
}

/// For the frame-rate line: once every 20 s, the probe's table, per frame.
pub(crate) fn line(frames: f64) {
    if !probe() {
        return;
    }
    static SINCE: Mutex<(Option<std::time::Instant>, f64)> = Mutex::new((None, 0.0));
    let mut since = SINCE.lock().unwrap_or_else(|e| e.into_inner());
    since.1 += frames;
    let started = *since.0.get_or_insert_with(std::time::Instant::now);
    if started.elapsed().as_secs() < 20 || since.1 <= 0.0 {
        return;
    }
    let frames = since.1;
    *since = (Some(std::time::Instant::now()), 0.0);
    drop(since);
    let s: Vec<f64> = STATS.iter().map(|v| v.swap(0, Relaxed) as f64 / frames).collect();
    let mb = |v: f64| v / 1e6;
    let (open, open_clean, open_bytes) = {
        let map = probe_map();
        let clean = map.values().filter(|w| w.touched == 0).count();
        let bytes: u64 = map.values().filter(|w| w.touched == 0).map(|w| w.merged).sum();
        (map.len(), clean, bytes)
    };
    let closed = s[CLEAN_CARD] + s[CLEAN_PC] + s[BY_TITLE] + s[BY_OTHER] + s[BY_IO];
    let clean_share = if closed > 0.0 { 100.0 * (s[CLEAN_CARD] + s[CLEAN_PC]) / closed } else { 0.0 };
    tracing::info!(
        "keep probe per frame (VICTORY_KEEP_PROBE=1): {:.0} windows opened by merges; {:.0} closed by the next GPU write \
         with no CPU touch ({clean_share:.0}% of {closed:.0} closed): {:.0} of card copies ({:.2} MB merged, {:.2} MB read \
         again by our refreshes and sent up again), {:.0} of PC copies ({:.2} MB merged, {:.2} MB read again); touched \
         first by the title's code {:.1}, by other code {:.1}, by a kernel write {:.1} ({:.2} MB merged in touched \
         windows); {:.1} dropped by remaps; {:.0} guard faults; {:.2} MB of our refresh reads fell in untouched windows. \
         Now open: {open} windows, {open_clean} untouched ({:.1} MB merged). Upper bound: touches during our own \
         reads are not seen",
        s[OPENED],
        s[CLEAN_CARD] + s[CLEAN_PC],
        s[CLEAN_CARD],
        mb(s[CLEAN_MERGED_CARD]),
        mb(s[CLEAN_ECHO_CARD]),
        s[CLEAN_PC],
        mb(s[CLEAN_MERGED_PC]),
        mb(s[CLEAN_ECHO_PC]),
        s[BY_TITLE],
        s[BY_OTHER],
        s[BY_IO],
        mb(s[TOUCHED_MERGED]),
        s[DROPPED],
        s[FAULTS],
        mb(s[ECHO_ALL]),
        open_bytes as f64 / 1e6,
    );
    let per = PER_COPY.lock().unwrap_or_else(|e| e.into_inner());
    let mut rows: Vec<(&(u64, u64), &[u64; 4])> = per.iter().collect();
    rows.sort_unstable_by_key(|(_, t)| std::cmp::Reverse(t[1]));
    let clean: Vec<String> = rows
        .iter()
        .take(10)
        .filter(|(_, t)| t[0] > 0)
        .map(|((at, len), t)| format!("{at:#x}+{len:#x}: {} clean ({:.1} MB), {} title, {} other", t[0], t[1] as f64 / 1e6, t[2], t[3]))
        .collect();
    rows.sort_unstable_by_key(|(_, t)| std::cmp::Reverse(t[2]));
    let touched: Vec<String> = rows
        .iter()
        .take(10)
        .filter(|(_, t)| t[2] > 0)
        .map(|((at, len), t)| format!("{at:#x}+{len:#x}: {} title, {} other, {} clean", t[2], t[3], t[0]))
        .collect();
    tracing::info!(clean = ?clean, touched = ?touched, "keep probe since the start: copies whose windows closed untouched most (bytes), and touched by the title most");
}
