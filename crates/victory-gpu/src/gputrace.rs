//! Diagnostics: the card's timeline (`VICTORY_GPU_TRACE=1`, diagnostics
//! builds). Every submission to the queue is wrapped in two lists of ours
//! that each write a timestamp (recorded once, at the start, one per slot
//! of a ring), so the card says when it began and ended each submission;
//! nothing is waited for and nothing is run alone (`VICTORY_GPU_TIMING`
//! runs every list by itself, which is not the schedule being measured).
//! The timestamps are read back long after they were written, put on the
//! CPU's clock (`GetClockCalibration`), and every 20 s logged by kind of
//! submission: the card's time in them a frame, how long each waited in
//! the queue from its submission to its start, and the card's idle time.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// Slots of the ring (two a submission).
const SLOTS: u32 = 8192;

struct Tracer {
    lists: Vec<ID3D12CommandList>,
    /// The resolved timestamps, mapped.
    data: *const u64,
    _back: ID3D12Resource,
    _heap: ID3D12QueryHeap,
    _allocator: ID3D12CommandAllocator,
    next: AtomicU32,
    /// Submissions traced, not yet read: kind, first slot, CPU time of the
    /// submission (QPC ticks).
    spans: Mutex<Vec<(&'static str, u32, i64)>>,
}

// SAFETY: D3D12 objects are free-threaded; the mapping is only read.
unsafe impl Send for Tracer {}
// SAFETY: as above.
unsafe impl Sync for Tracer {}

fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GPU_TRACE").is_some_and(|v| v == "1"))
}

fn tracer() -> Option<&'static Tracer> {
    static T: OnceLock<Option<Tracer>> = OnceLock::new();
    if !on() {
        return None;
    }
    T.get_or_init(|| {
        let made = make();
        match made {
            Ok(t) => {
                let _ = std::thread::Builder::new().name("victory gpu trace".into()).spawn(|| loop {
                    std::thread::sleep(std::time::Duration::from_secs(20));
                    report();
                });
                Some(t)
            }
            Err(e) => {
                tracing::warn!(error = %e, "no GPU timeline trace");
                None
            }
        }
    })
    .as_ref()
}

fn make() -> windows::core::Result<Tracer> {
    let host = crate::host::host().ok_or_else(windows::core::Error::empty)?;
    let device = &host.device;
    // SAFETY: plain object creation with valid descriptions.
    unsafe {
        let mut heap: Option<ID3D12QueryHeap> = None;
        device.CreateQueryHeap(
            &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: SLOTS, NodeMask: 0 },
            &mut heap,
        )?;
        let heap = heap.ok_or_else(windows::core::Error::empty)?;
        let props = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: u64::from(SLOTS) * 8,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut back: Option<ID3D12Resource> = None;
        device.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut back)?;
        let back = back.ok_or_else(windows::core::Error::empty)?;
        let mut data = std::ptr::null_mut();
        back.Map(0, None, Some(&mut data))?;
        let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
        let mut lists = Vec::with_capacity(SLOTS as usize);
        for slot in 0..SLOTS {
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
            list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, slot);
            list.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, slot, 1, &back, u64::from(slot) * 8);
            list.Close()?;
            lists.push(list.cast()?);
        }
        tracing::info!(slots = SLOTS, "GPU timeline trace on (VICTORY_GPU_TRACE=1)");
        Ok(Tracer {
            lists,
            data: data as *const u64,
            _back: back,
            _heap: heap,
            _allocator: allocator,
            next: AtomicU32::new(0),
            spans: Mutex::new(Vec::new()),
        })
    }
}

fn qpc() -> i64 {
    let mut t = 0i64;
    // SAFETY: a valid out pointer.
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut t) };
    t
}

/// `lists` with a timestamp list before and after them, for one submission
/// of kind `kind` about to be sent (as is without the trace).
pub(crate) fn wrap(kind: &'static str, lists: &[Option<ID3D12CommandList>]) -> Vec<Option<ID3D12CommandList>> {
    let Some(t) = tracer() else {
        return lists.to_vec();
    };
    let first = t.next.fetch_add(2, Ordering::Relaxed) % SLOTS;
    let mut out = Vec::with_capacity(lists.len() + 2);
    out.push(Some(t.lists[first as usize].clone()));
    out.extend_from_slice(lists);
    out.push(Some(t.lists[(first + 1) as usize % SLOTS as usize].clone()));
    t.spans.lock().unwrap_or_else(|e| e.into_inner()).push((kind, first, qpc()));
    out
}

fn report() {
    let Some(t) = tracer() else { return };
    let Some(host) = crate::host::host() else { return };
    // Spans older than 3 s have run (the card is never that far behind);
    // the ring turns over in far more than that.
    let now = qpc();
    let mut freq = 0i64;
    // SAFETY: a valid out pointer.
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut freq) };
    let freq = freq.max(1);
    let spans: Vec<(&'static str, u32, i64)> = {
        let mut all = t.spans.lock().unwrap_or_else(|e| e.into_inner());
        let cut = now - 3 * freq;
        let (old, young): (Vec<_>, Vec<_>) = all.drain(..).partition(|s| s.2 < cut);
        *all = young;
        old
    };
    if spans.is_empty() {
        return;
    }
    // GPU ticks to QPC ticks.
    let (mut gpu0, mut cpu0) = (0u64, 0u64);
    // SAFETY: valid out pointers; the host's queue.
    let gpu_freq = unsafe {
        let _ = host.queue.GetClockCalibration(&mut gpu0, &mut cpu0);
        host.queue.GetTimestampFrequency().unwrap_or(1)
    }
    .max(1);
    let to_qpc = |g: u64| cpu0 as f64 + (g as f64 - gpu0 as f64) * freq as f64 / gpu_freq as f64;
    let ms = |ticks: f64| ticks * 1000.0 / freq as f64;
    // SAFETY: inside the mapping; written by the card long ago.
    let read = |slot: u32| unsafe { std::ptr::read_volatile(t.data.add((slot % SLOTS) as usize)) };
    let mut by: std::collections::BTreeMap<&'static str, (u64, f64, f64, f64)> = Default::default();
    let mut runs: Vec<(f64, f64)> = Vec::new();
    for &(kind, first, submitted) in &spans {
        let (b, e) = (read(first), read(first + 1));
        if b == 0 || e < b {
            continue;
        }
        let (b, e) = (to_qpc(b), to_qpc(e));
        let entry = by.entry(kind).or_default();
        entry.0 += 1;
        entry.1 += ms(e - b);
        entry.2 += ms((b - submitted as f64).max(0.0));
        entry.3 = entry.3.max(ms((b - submitted as f64).max(0.0)));
        runs.push((b, e));
    }
    if runs.is_empty() {
        return;
    }
    runs.sort_by(|x, y| x.0.total_cmp(&y.0));
    let (mut busy, mut idle, mut end) = (0.0, 0.0, runs[0].0);
    for &(b, e) in &runs {
        if b > end {
            idle += b - end;
        }
        busy += (e - b.max(end)).max(0.0);
        end = end.max(e);
    }
    let window = ms(end - runs[0].0).max(1.0);
    let rows: Vec<String> = by
        .iter()
        .map(|(k, (n, run, wait, most))| {
            format!(
                "{k}: {n} sent, {:.1} ms on the card each ({run:.0} ms in all), {:.1} ms in the queue before starting each (longest {most:.0})",
                run / *n as f64,
                wait / *n as f64
            )
        })
        .collect();
    tracing::info!(
        "GPU timeline over {:.1} s: the card busy {:.0}% ({:.0} ms), idle {:.0} ms; {}",
        window / 1000.0,
        ms(busy) * 100.0 / window,
        ms(busy),
        ms(idle),
        rows.join("; ")
    );
}
