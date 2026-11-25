//! Diagnostics: the live profiler (`VICTORY_LIVE=1`, diagnostics builds).
//! Once per presented frame, one line of JSON with what that frame did:
//! the wall time since the last present, and the frame's share of the
//! counters the periodic log lines print per frame (copystat's [`C`]
//! counters, the host's submission, settle, relay and card-clock counters,
//! card copies, draws, pipelines, page faults), plus the latency of fence
//! signals sent behind flights, of the title's fence waits and of queue
//! waits held on the CPU. Sent by UDP to 127.0.0.1:47800
//! (`VICTORY_LIVE_ADDR` for another), never waited for (a line that cannot
//! go is dropped), and appended to `VICTORY_LIVE_FILE` if that is set.
//! `tools/vx-live.py` shows them.
//!
//! Names say their unit: `_ms` milliseconds, `_mb` megabytes (10^6 bytes),
//! `_pct` percent, anything else a count. `now_` fields are levels at the
//! present, not the frame's share. Summed over every thread, so at times
//! more than the frame's wall time: `record_ms`, `pipeline_wait_ms`,
//! `host_list_ms`, `list_reset_ms`, `note_ms`, `dyn_ms`, `pace_ms`,
//! `value_check_ms`, `tex_prep_ms`, `journal_lock_ms`, `guard_lock_ms`,
//! `guard_protect_ms`, `cpu_user_ms`, `cpu_kernel_ms`.
//!
//! The counters the log lines reset (`swap(0)`) are not taken from them:
//! copystat's are kept as totals beside the resets ([`crate::copystat::totals`]);
//! the host's are read at the present ([`frame`]) and their baseline moved
//! where the log line reset them ([`rebase`], called right after). What
//! another thread adds between the two calls to a counter that was reset
//! there (some microseconds, once a second) is not counted.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::SyncSender;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::copystat::C;

/// Whether the profiler runs (`VICTORY_LIVE=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LIVE").is_some_and(|v| v == "1"))
}

/// Microseconds since the first call.
fn now_us() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

// Where each counter sits in a reading ([`read`]). From `RESET` to
// `RESET_END` the ones a log line resets; the rest only grow.
const COPY: usize = 0;
const COPY_N: usize = 96;
const SUB: usize = COPY + COPY_N;
const RESET: usize = SUB;
const BACK: usize = SUB + 2;
const CLOCK: usize = BACK + 2;
const PREP: usize = CLOCK + 3;
const LATE: usize = PREP + 4;
const WAITED: usize = LATE + 18;
const RELAY: usize = WAITED + 11;
const CARD: usize = RELAY + 5;
const ECHO: usize = CARD + 7;
const DROPPED: usize = ECHO + 5;
const RESET_END: usize = DROPPED + 1;
const DRAWN: usize = RESET_END;
const DISPATCHED: usize = DRAWN + 1;
const HEAP_TEX: usize = DISPATCHED + 1;
const HEAP_TEX_BYTES: usize = HEAP_TEX + 1;
const PIPES_MADE: usize = HEAP_TEX_BYTES + 1;
const PIPES_LOADED: usize = PIPES_MADE + 1;
const FAULTS: usize = PIPES_LOADED + 1;
const CPU_USER: usize = FAULTS + 1;
const CPU_KERNEL: usize = CPU_USER + 1;
const N: usize = CPU_KERNEL + 1;

const US: f64 = 1e-3;
const NS: f64 = 1e-6;
const MB: f64 = 1e-6;

/// The fields that are one counter's growth over the frame: name, place
/// in a reading, scale (1: a count).
const FIELDS: &[(&str, usize, f64)] = &[
    // Recording (list calls; summed over the title's recording threads).
    ("record_calls", COPY + C::RecordCalls as usize, 1.0),
    ("record_ms", COPY + C::RecordNs as usize, NS),
    ("pipeline_wait_ms", COPY + C::PipelineWaitUs as usize, US),
    ("host_lists", COPY + C::HostLists as usize, 1.0),
    ("host_list_ms", COPY + C::HostListNs as usize, NS),
    ("list_reset_ms", COPY + C::ResetNs as usize, NS),
    ("note_ms", COPY + C::NoteNs as usize, NS),
    ("dyn_tables", COPY + C::DynamicAsked as usize, 1.0),
    ("dyn_refused", COPY + C::DynamicRefused as usize, 1.0),
    ("dyn_ms", COPY + C::DynamicNs as usize, NS),
    ("until_record_ms", COPY + C::UntilRecordUs as usize, US),
    ("until_submit_ms", COPY + C::UntilSubmitUs as usize, US),
    ("tail_ms", COPY + C::TailUs as usize, US),
    ("pace_waits", COPY + C::PaceWaits as usize, 1.0),
    ("pace_ms", COPY + C::PaceUs as usize, US),
    ("value_checks", COPY + C::WaitChecks as usize, 1.0),
    ("value_check_ms", COPY + C::WaitCheckNs as usize, NS),
    // Submissions: preparing them on the CPU, waiting for the card.
    ("submissions", COPY + C::Submissions as usize, 1.0),
    ("back_submissions", COPY + C::BackSubmissions as usize, 1.0),
    ("prep_ms", SUB, US),
    ("gpu_wait_ms", SUB + 1, US),
    ("copy_prep_ms", PREP, US),
    ("copy_prep_mb", PREP + 1, MB),
    ("tex_prep_ms", PREP + 2, US),
    ("tex_prep_mb", PREP + 3, MB),
    // Preparing, by part (submitting thread, settles left out).
    ("prep_ask_ms", COPY + C::PrepAskUs as usize, US),
    ("prep_steps_ms", COPY + C::PrepStepsUs as usize, US),
    ("prep_fill_ms", COPY + C::PrepFillNs as usize, NS),
    ("prep_gpu_write_ms", COPY + C::PrepGpuWriteUs as usize, US),
    ("prep_flush_ms", COPY + C::PrepFlushUs as usize, US),
    ("prep_card_ms", COPY + C::PrepCardUs as usize, US),
    ("copy_back_ms", BACK, US),
    ("copy_back_mb", BACK + 1, MB),
    ("nothing_back", COPY + C::NothingBack as usize, 1.0),
    ("nothing_back_ms", COPY + C::NothingBackUs as usize, US),
    ("forced_held", COPY + C::Forced as usize, 1.0),
    // The card's clock.
    ("card_idle_ms", CLOCK, US),
    ("card_idle_behind_ms", CLOCK + 1, US),
    ("card_stood_ms", CLOCK + 2, US),
    // Settles of flights sent ahead, and why.
    ("sent_ahead", LATE, 1.0),
    ("settles", LATE + 1, 1.0),
    ("settles_sync", LATE + 2, 1.0),
    ("settles_card_done", LATE + 3, 1.0),
    ("settles_value", LATE + 4, 1.0),
    ("settles_stale", LATE + 5, 1.0),
    ("settles_read", LATE + 6, 1.0),
    ("settles_run", LATE + 7, 1.0),
    ("settles_most", LATE + 8, 1.0),
    ("settles_writes", LATE + 9, 1.0),
    ("settles_fence", LATE + 10, 1.0),
    ("settled_flights", LATE + 11, 1.0),
    ("settle_ms", LATE + 12, US),
    ("value_waits_met_by_flight", LATE + 13, 1.0),
    ("late_signals", LATE + 14, 1.0),
    ("settles_read_copy", LATE + 15, 1.0),
    ("settles_read_fill", LATE + 16, 1.0),
    ("settles_read_texture", LATE + 17, 1.0),
    ("settle_sync_ms", WAITED + 2, US),
    ("settle_card_done_ms", WAITED + 3, US),
    ("settle_value_ms", WAITED + 4, US),
    ("settle_stale_ms", WAITED + 5, US),
    ("settle_read_ms", WAITED + 6, US),
    ("settle_run_ms", WAITED + 7, US),
    ("settle_most_ms", WAITED + 8, US),
    ("settle_writes_ms", WAITED + 9, US),
    ("settle_fence_ms", WAITED + 10, US),
    // The relay (the thread that lands flights).
    ("relay_landings", RELAY, 1.0),
    ("relay_flights", RELAY + 1, 1.0),
    ("relay_idle_ms", RELAY + 2, US),
    ("relay_fetch_ms", RELAY + 3, US),
    ("relay_landing_ms", RELAY + 4, US),
    // Title memory copies, write watch, guards.
    ("copies", COPY + C::Copies as usize, 1.0),
    ("copy_mb", COPY + C::CopyBytes as usize, MB),
    ("refreshes", COPY + C::Refreshes as usize, 1.0),
    ("refresh_mb", COPY + C::RefreshBytes as usize, MB),
    ("skipped", COPY + C::Skipped as usize, 1.0),
    ("skipped_mb", COPY + C::SkippedBytes as usize, MB),
    ("runs", COPY + C::Runs as usize, 1.0),
    ("run_ms", COPY + C::RunUs as usize, US),
    ("queries", COPY + C::Queries as usize, 1.0),
    ("query_ms", COPY + C::QueryUs as usize, US),
    ("move_ms", COPY + C::MoveUs as usize, US),
    ("unmapped_mb", COPY + C::Unmapped as usize, MB),
    ("watches", COPY + C::Watches as usize, 1.0),
    ("watch_ms", COPY + C::WatchUs as usize, US),
    ("watch_fails", COPY + C::WatchFails as usize, 1.0),
    ("watch_fail_ms", COPY + C::WatchFailUs as usize, US),
    ("ask_skipped_views", COPY + C::AskSkippedViews as usize, 1.0),
    ("journal_lock_ms", COPY + C::JournalLockNs as usize, NS),
    ("checked_mb", COPY + C::Checked as usize, MB),
    ("unchanged_mb", COPY + C::Unchanged as usize, MB),
    ("fallbacks", COPY + C::Fallbacks as usize, 1.0),
    ("guard_faults", COPY + C::GuardFaults as usize, 1.0),
    ("guard_ms", COPY + C::GuardUs as usize, US),
    ("guard_lock_ms", COPY + C::GuardLockNs as usize, NS),
    ("guard_slow", COPY + C::GuardSlow as usize, 1.0),
    ("guard_protect_ms", COPY + C::GuardProtectNs as usize, NS),
    ("sparse_ms", COPY + C::SparseUs as usize, US),
    ("whole_mb", COPY + C::WholeBytes as usize, MB),
    ("whole_ms", COPY + C::WholeUs as usize, US),
    ("dirty_mb", COPY + C::DirtyKernelPages as usize, 4096.0 * MB),
    ("dirty_units_mb", COPY + C::DirtyUnits as usize, 65536.0 * MB),
    // What shaders wrote, found and put back.
    ("gpu_write_steps", COPY + C::GpuWrites as usize, 1.0),
    ("snapshot_ms", COPY + C::SnapshotUs as usize, US),
    ("back_compared_mb", COPY + C::BackCompared as usize, MB),
    ("back_scan_ms", COPY + C::BackScanUs as usize, US),
    ("back_differ", COPY + C::BackDiffer as usize, 1.0),
    ("back_sync_ms", COPY + C::BackSyncUs as usize, US),
    // Copies in the card's memory.
    ("card_up_mb", CARD, MB),
    ("card_up_ranges", CARD + 1, 1.0),
    ("card_pieces_compared", CARD + 2, 1.0),
    ("card_pieces_fetched", CARD + 3, 1.0),
    ("card_fetch_mb", CARD + 4, MB),
    ("card_fetch_waits", CARD + 5, 1.0),
    ("card_fetch_ms", CARD + 6, US),
    // Round trips of what shaders wrote (`crate::shadow::keep_echo`).
    ("card_refresh_mb", ECHO, MB),
    ("echo_mb", ECHO + 1, MB),
    ("echo_same_mb", ECHO + 2, MB),
    ("echo_pieces", ECHO + 3, 1.0),
    ("echo_skipped_mb", ECHO + 4, MB),
    ("card_pieces_wide", COPY + C::CardPiecesWide as usize, 1.0),
    // Textures.
    ("tex_uploads", COPY + C::TexUploads as usize, 1.0),
    ("tex_skipped", COPY + C::TexSkipped as usize, 1.0),
    ("tex_watched_mb", COPY + C::TexWatched as usize, MB),
    ("tex_hashed_mb", COPY + C::TexHashed as usize, MB),
    ("tex_changed_mb", COPY + C::TexChanged as usize, MB),
    ("heap_tex_uploads", HEAP_TEX, 1.0),
    ("heap_tex_mb", HEAP_TEX_BYTES, MB),
    // Pipelines, draws.
    ("pipelines_made", PIPES_MADE, 1.0),
    ("pipelines_loaded", PIPES_LOADED, 1.0),
    ("draws", DRAWN, 1.0),
    ("dispatches", DISPATCHED, 1.0),
    ("dropped", DROPPED, 1.0),
    ("barrier_waits_groups", COPY + C::WaitsForGroups as usize, 1.0),
    ("barrier_waits_buffers", COPY + C::WaitsForBuffers as usize, 1.0),
    ("indirect_bounded", COPY + C::IndirectBounded as usize, 1.0),
    ("indirect_covered", COPY + C::IndirectCovered as usize, 1.0),
    ("indirect_from_window", COPY + C::IndirectFromWindow as usize, 1.0),
    ("indirect_window_cut", COPY + C::IndirectWindowCut as usize, 1.0),
    // The process.
    ("page_faults", FAULTS, 1.0),
    ("cpu_user_ms", CPU_USER, US),
    ("cpu_kernel_ms", CPU_KERNEL, US),
];

/// Every counter, as it stands (`p`: the process now).
fn read(p: &Process) -> [u64; N] {
    let mut r = [0u64; N];
    let totals = crate::copystat::totals();
    for (to, from) in r[COPY..COPY + COPY_N].iter_mut().zip(&totals) {
        *to = *from;
    }
    read_reset(&mut r);
    r[DRAWN] = crate::draw::drawn();
    r[DISPATCHED] = crate::draw::dispatched();
    r[HEAP_TEX] = crate::draw::HEAP_TEXTURES.load(Relaxed);
    r[HEAP_TEX_BYTES] = crate::draw::HEAP_TEXTURE_BYTES.load(Relaxed);
    [r[PIPES_MADE], r[PIPES_LOADED]] = crate::pipeline::made_and_loaded();
    r[FAULTS] = p.faults;
    r[CPU_USER] = p.user_us;
    r[CPU_KERNEL] = p.kernel_us;
    r
}

/// The counters a log line resets, into their places of `r`.
fn read_reset(r: &mut [u64; N]) {
    use crate::host;
    let mut copy = |at: usize, from: &[AtomicU64]| {
        for (i, a) in from.iter().enumerate() {
            r[at + i] = a.load(Relaxed);
        }
    };
    copy(SUB, &host::SUBMISSION_US);
    copy(BACK, &host::COPY_BACK);
    copy(CLOCK, &host::CARD_CLOCK_US);
    copy(PREP, &host::PREPARING);
    copy(LATE, &host::LATE_STATS);
    copy(WAITED, &host::LATE_WAITED);
    copy(RELAY, &host::RELAY_STATS);
    copy(CARD, &crate::shadow::CARD_STATS);
    copy(ECHO, &crate::shadow::ECHO_STATS);
    copy(DROPPED, std::slice::from_ref(&crate::draw::DROPPED));
}

/// The process's page faults so far, memory now and processor time so far.
struct Process {
    faults: u64,
    working: u64,
    private: u64,
    user_us: u64,
    kernel_us: u64,
}

fn process() -> Process {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: our own process, a sized structure and four valid out pointers.
    unsafe {
        K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb);
        GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user);
    }
    let micros = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime)) / 10;
    Process {
        faults: u64::from(c.PageFaultCount),
        working: c.WorkingSetSize as u64,
        private: c.PagefileUsage as u64,
        user_us: micros(user),
        kernel_us: micros(kernel),
    }
}

/// Count, sum and longest of some waits over a frame (microseconds).
#[derive(Default, Clone, Copy)]
struct Waits {
    n: u64,
    sum: u64,
    max: u64,
}

impl Waits {
    fn add(&mut self, us: u64) {
        self.n += 1;
        self.sum += us;
        self.max = self.max.max(us);
    }
}

/// What the hooks in fence.rs, queue.rs and host.rs saw: what is pending
/// (with when it began, [`now_us`]) and the frame's waits so far.
#[derive(Default)]
struct Track {
    /// Fence signals sent behind flights and not set yet: fence, value.
    signals: Vec<(usize, u64, u64)>,
    /// Of those, when the card was seen to have run what each was behind.
    signals_gpu: Vec<(usize, u64, u64)>,
    /// Title fence waits (events) not set yet: fence, value.
    waiters: Vec<(usize, u64, u64)>,
    /// Queue waits held on the CPU: queue, fence, value.
    held: Vec<(usize, usize, u64, u64)>,
    signals_queued: u64,
    signals_set: Waits,
    /// Of `signals_set`: from queued to the card having run what it was
    /// behind, and from then to set (the landing).
    signals_to_gpu: Waits,
    signals_gpu_to_set: Waits,
    waiters_added: u64,
    waiters_met: u64,
    waiters_set: Waits,
    held_added: u64,
    held_released: Waits,
    /// Pending entries given up on (fences released, values never reached).
    expired: u64,
}

/// Entries older than this are dropped ([`Track::expired`]).
const EXPIRE_US: u64 = 60_000_000;
/// The most pending entries of each kind kept.
const MOST: usize = 4096;

/// Taken last: nothing else is locked while this is held.
static TRACK: Mutex<Option<Track>> = Mutex::new(None);

fn track<R>(f: impl FnOnce(&mut Track) -> R) -> R {
    let mut t = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    f(t.get_or_insert_with(Track::default))
}

/// Keeps `v` within [`MOST`] entries, dropping the oldest.
fn cap<T>(v: &mut Vec<T>, expired: &mut u64) {
    if v.len() >= MOST {
        let drop = v.len() + 1 - MOST;
        v.drain(..drop);
        *expired += drop as u64;
    }
}

/// A fence signal went behind the flights in the air (host.rs
/// `late_signal`, under the line's lock).
pub(crate) fn signal_queued(fence: usize, value: u64) {
    if !on() {
        return;
    }
    let now = now_us();
    track(|t| {
        cap(&mut t.signals, &mut t.expired);
        t.signals.push((fence, value, now));
        t.signals_queued += 1;
    });
}

/// The card has run what a queued fence signal was behind (host.rs, the
/// relay taking it off the line to land it).
pub(crate) fn signal_gpu_done(fence: usize, value: u64) {
    if !on() {
        return;
    }
    let now = now_us();
    track(|t| {
        cap(&mut t.signals_gpu, &mut t.expired);
        t.signals_gpu.push((fence, value, now));
    });
}

/// The title waits for a fence with an event (fence.rs, under the fence's
/// waiter lock), the value not reached yet.
pub(crate) fn waiter_added(fence: usize, value: u64) {
    if !on() {
        return;
    }
    let now = now_us();
    track(|t| {
        cap(&mut t.waiters, &mut t.expired);
        t.waiters.push((fence, value, now));
        t.waiters_added += 1;
    });
}

/// The title asked to wait for a fence value reached already.
pub(crate) fn waiter_met() {
    if on() {
        track(|t| t.waiters_met += 1);
    }
}

/// A fence was set to `value` (fence.rs, under the fence's waiter lock):
/// the signal queued for it has landed and the waits it reaches end.
pub(crate) fn fence_set(fence: usize, value: u64) {
    if !on() {
        return;
    }
    let now = now_us();
    track(|t| {
        if let Some(at) = t.signals.iter().position(|s| s.0 == fence && s.1 == value) {
            let (_, _, since) = t.signals.remove(at);
            t.signals_set.add(now.saturating_sub(since));
            if let Some(g) = t.signals_gpu.iter().position(|s| s.0 == fence && s.1 == value) {
                let (_, _, done) = t.signals_gpu.remove(g);
                t.signals_to_gpu.add(done.saturating_sub(since));
                t.signals_gpu_to_set.add(now.saturating_sub(done));
            }
        }
        let Track { waiters, waiters_set, .. } = t;
        waiters.retain(|w| {
            let reached = w.0 == fence && w.1 <= value;
            if reached {
                waiters_set.add(now.saturating_sub(w.2));
            }
            !reached
        });
    });
}

/// A queue holds what it is given until `fence` reaches `value` (queue.rs,
/// under the queue's lock).
pub(crate) fn held_wait(queue: usize, fence: usize, value: u64) {
    if !on() {
        return;
    }
    let now = now_us();
    track(|t| {
        cap(&mut t.held, &mut t.expired);
        t.held.push((queue, fence, value, now));
        t.held_added += 1;
    });
}

/// That wait of the queue's was passed (its fence reached, or at a present).
pub(crate) fn held_released(queue: usize, fence: usize, value: u64) {
    if !on() {
        return;
    }
    let now = now_us();
    track(|t| {
        if let Some(at) = t.held.iter().position(|h| h.0 == queue && h.1 == fence && h.2 == value) {
            let (.., since) = t.held.remove(at);
            t.held_released.add(now.saturating_sub(since));
        }
    });
}

/// When the title's present began (queue.rs; [`now_us`]).
static PRESENT_BEGAN: AtomicU64 = AtomicU64::new(0);

/// The title calls its present.
pub(crate) fn present_begins() {
    if on() {
        PRESENT_BEGAN.store(now_us(), Relaxed);
    }
}

/// What [`frame`] keeps from one present to the next.
struct State {
    /// The reading at the last present, moved by [`rebase`].
    prev: [u64; N],
    /// When the last present was counted ([`now_us`]).
    at: u64,
    frames: u64,
    /// The last present's log lines took this long ([`logged`]).
    log_us: u64,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn state() -> std::sync::MutexGuard<'static, Option<State>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Takes the lines to the thread that sends them; `None` if it did not start.
fn sender() -> Option<&'static SyncSender<String>> {
    static TX: OnceLock<Option<SyncSender<String>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(256);
        let addr = std::env::var("VICTORY_LIVE_ADDR").unwrap_or_else(|_| "127.0.0.1:47800".into());
        let file = std::env::var_os("VICTORY_LIVE_FILE");
        let spawned = std::thread::Builder::new().name("victory-live".into()).spawn(move || {
            let socket = std::net::UdpSocket::bind("127.0.0.1:0").ok();
            if let Some(s) = &socket {
                let _ = s.set_nonblocking(true);
            }
            let mut out = file.and_then(|path| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .inspect_err(|e| tracing::warn!(error = %e, ?path, "live profiler: no file"))
                    .ok()
                    .map(std::io::BufWriter::new)
            });
            tracing::info!(%addr, file = out.is_some(), "live profiler: a line per frame");
            while let Ok(line) = rx.recv() {
                if let Some(s) = &socket {
                    let _ = s.send_to(line.as_bytes(), addr.as_str());
                }
                if let Some(w) = out.as_mut() {
                    use std::io::Write;
                    let _ = w.write_all(line.as_bytes());
                    let _ = w.write_all(b"\n");
                    let _ = w.flush();
                }
            }
        });
        match spawned {
            Ok(_) => Some(tx),
            Err(e) => {
                tracing::warn!(error = %e, "live profiler: no thread");
                None
            }
        }
    })
    .as_ref()
}

/// A frame was presented (host.rs `Meter::frame`, before its log lines):
/// sends its line.
pub(crate) fn frame() {
    if !on() {
        return;
    }
    let now = now_us();
    let p = process();
    let reading = read(&p);
    let line = {
        let mut s = state();
        let Some(st) = s.as_mut() else {
            *s = Some(State { prev: reading, at: now, frames: 0, log_us: 0 });
            return;
        };
        st.frames += 1;
        let began = PRESENT_BEGAN.load(Relaxed);
        let present_us = if began > st.at { now.saturating_sub(began) } else { 0 };
        let mut delta = [0u64; N];
        for i in 0..N {
            delta[i] = reading[i].saturating_sub(st.prev[i]);
        }
        let head = Head {
            frame: st.frames,
            frame_us: now.saturating_sub(st.at),
            present_us,
            log_us: std::mem::take(&mut st.log_us),
        };
        st.prev = reading;
        st.at = now;
        (head, delta)
    };
    let (gauges, waits) = track(|t| {
        let old = now.saturating_sub(EXPIRE_US);
        let before = t.signals.len() + t.waiters.len() + t.held.len();
        t.signals.retain(|s| s.2 >= old);
        t.waiters.retain(|w| w.2 >= old);
        t.held.retain(|h| h.3 >= old);
        t.expired += (before - t.signals.len() - t.waiters.len() - t.held.len()) as u64;
        let oldest = |at: Option<u64>| at.map_or(0, |at| now.saturating_sub(at));
        let gauges = [
            t.signals.len() as u64,
            oldest(t.signals.iter().map(|s| s.2).min()),
            t.waiters.len() as u64,
            oldest(t.waiters.iter().map(|w| w.2).min()),
            t.held.len() as u64,
            oldest(t.held.iter().map(|h| h.3).min()),
        ];
        let waits = Frame {
            signals_queued: std::mem::take(&mut t.signals_queued),
            signals_set: std::mem::take(&mut t.signals_set),
            signals_to_gpu: std::mem::take(&mut t.signals_to_gpu),
            signals_gpu_to_set: std::mem::take(&mut t.signals_gpu_to_set),
            waiters_added: std::mem::take(&mut t.waiters_added),
            waiters_met: std::mem::take(&mut t.waiters_met),
            waiters_set: std::mem::take(&mut t.waiters_set),
            held_added: std::mem::take(&mut t.held_added),
            held_released: std::mem::take(&mut t.held_released),
            expired: std::mem::take(&mut t.expired),
        };
        (gauges, waits)
    });
    let text = line_text(&line.0, &line.1, &waits, &gauges, &p);
    // A full channel (the sending thread held up): the line is dropped.
    if let Some(tx) = sender() {
        let _ = tx.try_send(text);
    }
}

/// After the log lines' resets (host.rs `Meter::frame`): where a counter
/// was reset, its baseline is what it holds now.
pub(crate) fn rebase() {
    if !on() {
        return;
    }
    let mut now = [0u64; N];
    read_reset(&mut now);
    let mut s = state();
    if let Some(st) = s.as_mut() {
        for i in RESET..RESET_END {
            if now[i] < st.prev[i] {
                st.prev[i] = now[i];
            }
        }
    }
}

/// The present's log lines are done (host.rs `Meter::frame`): their time
/// goes with the next frame.
pub(crate) fn logged() {
    if !on() {
        return;
    }
    let now = now_us();
    let mut s = state();
    if let Some(st) = s.as_mut() {
        st.log_us = now.saturating_sub(st.at);
    }
}

struct Head {
    frame: u64,
    frame_us: u64,
    present_us: u64,
    log_us: u64,
}

/// The frame's waits from [`Track`].
struct Frame {
    signals_queued: u64,
    signals_set: Waits,
    /// Of `signals_set`: from queued to the card having run what it was
    /// behind, and from then to set (the landing).
    signals_to_gpu: Waits,
    signals_gpu_to_set: Waits,
    waiters_added: u64,
    waiters_met: u64,
    waiters_set: Waits,
    held_added: u64,
    held_released: Waits,
    expired: u64,
}

/// Appends `"name":value` with `decimals` places (non-finite values as 0).
fn put(out: &mut String, name: &str, value: f64, decimals: usize) {
    let value = if value.is_finite() { value } else { 0.0 };
    if !out.ends_with('{') {
        out.push(',');
    }
    let _ = write!(out, "\"{name}\":{value:.decimals$}");
}

/// The frame's line of JSON.
fn line_text(head: &Head, d: &[u64; N], w: &Frame, gauges: &[u64; 6], p: &Process) -> String {
    let mut out = String::with_capacity(4096);
    out.push('{');
    let ms = |us: u64| us as f64 * US;
    let frame_ms = ms(head.frame_us);
    put(&mut out, "f", head.frame as f64, 0);
    put(&mut out, "title_frame", crate::frame::current() as f64, 0);
    put(&mut out, "t", now_us() as f64 * 1e-6, 3);
    put(&mut out, "frame_ms", frame_ms, 3);
    put(&mut out, "present_ms", ms(head.present_us), 3);
    put(&mut out, "log_ms", ms(head.log_us), 3);
    // The card busy by the card clock: idle stretches are counted when the
    // next send ends them, so a frame's share can be off by one stretch.
    let idle = ms(d[CLOCK]);
    let busy = if frame_ms > 0.0 { (100.0 * (1.0 - idle / frame_ms)).clamp(0.0, 100.0) } else { 0.0 };
    put(&mut out, "gpu_busy_pct", busy, 1);
    // The waits for the card per submission, by length.
    let at = |c: C| d[COPY + c as usize];
    let waits = at(C::CardUnder2) + at(C::Card2To8) + at(C::Card8To32) + at(C::CardOver32);
    let waited = at(C::CardUnder2Us) + at(C::Card2To8Us) + at(C::Card8To32Us) + at(C::CardOver32Us);
    put(&mut out, "card_waits", waits as f64, 0);
    put(&mut out, "card_wait_ms", ms(waited), 3);
    put(&mut out, "card_long_waits", (at(C::Card8To32) + at(C::CardOver32)) as f64, 0);
    put(&mut out, "card_long_wait_ms", ms(at(C::Card8To32Us) + at(C::CardOver32Us)), 3);
    for &(name, i, scale) in FIELDS {
        let decimals = if scale == 1.0 { 0 } else { 3 };
        put(&mut out, name, d[i] as f64 * scale, decimals);
    }
    let mean = |w: &Waits| if w.n > 0 { ms(w.sum) / w.n as f64 } else { 0.0 };
    put(&mut out, "sig_queued", w.signals_queued as f64, 0);
    put(&mut out, "sig_set", w.signals_set.n as f64, 0);
    put(&mut out, "sig_mean_ms", mean(&w.signals_set), 3);
    put(&mut out, "sig_max_ms", ms(w.signals_set.max), 3);
    put(&mut out, "sig_to_gpu_ms", mean(&w.signals_to_gpu), 3);
    put(&mut out, "sig_gpu_to_set_ms", mean(&w.signals_gpu_to_set), 3);
    put(&mut out, "wait_added", w.waiters_added as f64, 0);
    put(&mut out, "wait_met_at_once", w.waiters_met as f64, 0);
    put(&mut out, "wait_done", w.waiters_set.n as f64, 0);
    put(&mut out, "wait_mean_ms", mean(&w.waiters_set), 3);
    put(&mut out, "wait_max_ms", ms(w.waiters_set.max), 3);
    put(&mut out, "wait_sum_ms", ms(w.waiters_set.sum), 3);
    put(&mut out, "held_added", w.held_added as f64, 0);
    put(&mut out, "held_done", w.held_released.n as f64, 0);
    put(&mut out, "held_mean_ms", mean(&w.held_released), 3);
    put(&mut out, "held_max_ms", ms(w.held_released.max), 3);
    put(&mut out, "held_sum_ms", ms(w.held_released.sum), 3);
    put(&mut out, "track_expired", w.expired as f64, 0);
    put(&mut out, "now_sig_pending", gauges[0] as f64, 0);
    put(&mut out, "now_sig_oldest_ms", ms(gauges[1]), 3);
    put(&mut out, "now_wait_pending", gauges[2] as f64, 0);
    put(&mut out, "now_wait_oldest_ms", ms(gauges[3]), 3);
    put(&mut out, "now_held_pending", gauges[4] as f64, 0);
    put(&mut out, "now_held_oldest_ms", ms(gauges[5]), 3);
    put(&mut out, "now_pipelines_queued", crate::pipeline::making() as f64, 0);
    put(&mut out, "now_ws_mb", p.working as f64 * MB, 1);
    put(&mut out, "now_private_mb", p.private as f64 * MB, 1);
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field has a name of its own, and every place is in a reading.
    #[test]
    fn fields_are_distinct_and_in_range() {
        let mut names: Vec<&str> = FIELDS.iter().map(|f| f.0).collect();
        names.sort_unstable();
        let all = names.len();
        names.dedup();
        assert_eq!(names.len(), all);
        assert!(FIELDS.iter().all(|f| f.1 < N));
        assert_eq!(crate::copystat::totals().len(), COPY_N);
    }

    /// A line is one JSON object of names and finite numbers.
    #[test]
    fn a_line_is_flat_json() {
        let mut d = [0u64; N];
        d[COPY + C::RecordNs as usize] = 2_500_000;
        d[CLOCK] = 5_000;
        let head = Head { frame: 7, frame_us: 20_000, present_us: 1_000, log_us: 0 };
        let w = Frame {
            signals_queued: 1,
            signals_set: Waits { n: 2, sum: 3_000, max: 2_000 },
            signals_to_gpu: Waits::default(),
            signals_gpu_to_set: Waits::default(),
            waiters_added: 0,
            waiters_met: 0,
            waiters_set: Waits::default(),
            held_added: 0,
            held_released: Waits::default(),
            expired: 0,
        };
        let p = Process { faults: 0, working: 0, private: 0, user_us: 0, kernel_us: 0 };
        let text = line_text(&head, &d, &w, &[0; 6], &p);
        assert!(text.starts_with("{\"f\":7,") && text.ends_with('}'));
        assert!(text.contains("\"record_ms\":2.500"));
        assert!(text.contains("\"gpu_busy_pct\":75.0"));
        assert!(text.contains("\"sig_mean_ms\":1.500"));
        assert!(!text.contains("NaN") && !text.contains("inf"));
    }
}
