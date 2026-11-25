//! Diagnostics: where the submission copies of title memory go (which
//! ranges, how often, of what memory) and what limits their rate, logged
//! with the frame-rate line. `VICTORY_COPY_MODE=plain` copies without the
//! mapped-page guard, `none` skips the copies (both for timing only).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Counters since the last line.
pub(crate) enum C {
    Submissions,
    Copies,
    CopyBytes,
    Refreshes,
    RefreshBytes,
    Skipped,
    SkippedBytes,
    Runs,
    RunUs,
    Queries,
    QueryUs,
    MoveUs,
    Unmapped,
    /// Write-watch askings and their time.
    Watches,
    WatchUs,
    /// Bytes of watched ranges checked, and of those found unchanged.
    Checked,
    Unchanged,
    /// Copies that met an unmapped page.
    Fallbacks,
    /// Writes taken by the guard on view memory.
    GuardFaults,
    /// Texture bytes detiled after a watched write; hashed; detiled after a
    /// hash change.
    TexWatched,
    TexHashed,
    TexChanged,
    WholeReadNs,
    FailedReadNs,
    RetryNs,
    ZeroNs,
    SinglePagePieces,
    WritebackUniqueRanges,
    WritebackCollectNs,
    WritebackMergeNs,
    WritebackMergeBytes,
    WritebackCompareNs,
    WritebackReadNs,
    WritebackWriteNs,
    WritebackWatchNs,
    WritebackWriteBytes,
    WritebackFirstOkNs,
    WritebackFirstFailedNs,
    WritebackRetryNs,
    WritebackFirstCalls,
    WritebackFailedCalls,
}
const COUNTERS: usize = C::WritebackFailedCalls as usize + 1;
static COUNT: [AtomicU64; COUNTERS] = [const { AtomicU64::new(0) }; COUNTERS];

pub(crate) fn add(c: C, n: u64) {
    COUNT[c as usize].fetch_add(n, Relaxed);
}

const PAGE: usize = 4096;
static REFRESH_GAPS: Mutex<[u64; 6]> = Mutex::new([0; 6]);

pub(crate) fn refresh_gaps_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("VICTORY_COPY_GAPS").is_some())
}

pub(crate) struct RefreshProbe {
    gaps: Vec<(usize, usize)>,
    pub(crate) span: usize,
    pub(crate) requested: usize,
    pub(crate) copied_gap: usize,
    pub(crate) watch_us: u64,
}

impl RefreshProbe {
    pub(crate) fn new(mut requests: Vec<(usize, usize)>) -> Self {
        requests.sort_unstable();
        let start = requests[0].0;
        let mut end = start;
        let mut requested = 0;
        let mut gaps = Vec::new();
        for (from, to) in requests {
            if from > end {
                let gap_start = end.saturating_add(PAGE - 1) & !(PAGE - 1);
                let gap_end = from & !(PAGE - 1);
                if gap_start < gap_end {
                    gaps.push((gap_start, gap_end));
                }
            }
            requested += to.saturating_sub(end.max(from));
            end = end.max(to);
        }
        Self {
            gaps,
            span: end - start,
            requested,
            copied_gap: 0,
            watch_us: 0,
        }
    }

    pub(crate) fn copy(&mut self, from: usize, to: usize) {
        self.copied_gap += self
            .gaps
            .iter()
            .skip_while(|&&(_, end)| end <= from)
            .take_while(|&&(start, _)| start < to)
            .map(|&(start, end)| to.min(end) - from.max(start))
            .sum::<usize>();
    }

    pub(crate) fn record(self, elapsed_us: u64) {
        if self.gaps.is_empty() {
            return;
        }
        let mut totals = REFRESH_GAPS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (total, value) in totals.iter_mut().zip([
            1,
            self.span as u64,
            self.requested as u64,
            self.copied_gap as u64,
            elapsed_us,
            self.watch_us,
        ]) {
            *total += value;
        }
    }
}

#[cfg(test)]
#[test]
fn refresh_probe_counts_only_unrequested_whole_pages() {
    let mut probe = RefreshProbe::new(vec![(3 * PAGE + 1, 4 * PAGE), (1, 40), (20, 80)]);
    assert_eq!(probe.span, 4 * PAGE - 1);
    assert_eq!(probe.requested, 79 + PAGE - 1);
    probe.copy(1, PAGE);
    assert_eq!(probe.copied_gap, 0);
    probe.copy(PAGE + 8, 2 * PAGE + 8);
    assert_eq!(probe.copied_gap, PAGE);
    assert_eq!(probe.watch_us, 0);
}

/// How copies run: 0 guarded (as always), 1 plain, 2 not at all.
pub(crate) fn mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_COPY_MODE").as_deref() {
        Ok("plain") => 1,
        Ok("none") => 2,
        _ => 0,
    })
}

/// Per source range (start, length): copies, bytes, microseconds, memory type.
type Ranges = HashMap<(usize, usize), (u64, u64, u64, u32)>;
static RANGES: Mutex<Option<Ranges>> = Mutex::new(None);

/// A copy of `len` bytes from `src` (memory type `kind`) took `us`.
pub(crate) fn range(src: usize, len: usize, us: u64, kind: u32) {
    let mut map = RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let e = map
        .get_or_insert_with(HashMap::new)
        .entry((src, len))
        .or_insert((0, 0, 0, kind));
    e.0 += 1;
    e.1 += len as u64;
    e.2 += us;
}

fn page_faults() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    u64::from(c.PageFaultCount)
}

/// Logs the counters per frame over `frames` frames, and the ranges that
/// took most bytes.
pub(crate) fn log(frames: f64) {
    if refresh_gaps_enabled() {
        let [flushes, span, requested, copied_gap, elapsed, watch] = std::mem::take(
            &mut *REFRESH_GAPS
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        tracing::info!(
            frames,
            flushes,
            span,
            requested,
            copied_gap,
            elapsed_us = elapsed,
            watch_us = watch,
            "shadow refreshes with whole-page gaps (window totals)"
        );
    }
    static FAULTS: AtomicU64 = AtomicU64::new(0);
    let faults = page_faults();
    let faults = faults - FAULTS.swap(faults, Relaxed).min(faults);
    let c: Vec<f64> = COUNT
        .iter()
        .map(|a| a.swap(0, Relaxed) as f64 / frames)
        .collect();
    if refresh_gaps_enabled() {
        tracing::info!(
            frames,
            whole_ms = c[C::WholeReadNs as usize] / 1e6,
            failed_ms = c[C::FailedReadNs as usize] / 1e6,
            retry_ms = c[C::RetryNs as usize] / 1e6,
            zero_ms = c[C::ZeroNs as usize] / 1e6,
            single_page_pieces = c[C::SinglePagePieces as usize],
            "mapped copy stages (ms/frame)"
        );
        tracing::info!(
            frames,
            unique_ranges = c[C::WritebackUniqueRanges as usize],
            collect_ms = c[C::WritebackCollectNs as usize] / 1e6,
            merge_ms = c[C::WritebackMergeNs as usize] / 1e6,
            merge_mb = c[C::WritebackMergeBytes as usize] / 1e6,
            compare_ms = c[C::WritebackCompareNs as usize] / 1e6,
            read_ms = c[C::WritebackReadNs as usize] / 1e6,
            write_ms = c[C::WritebackWriteNs as usize] / 1e6,
            watch_ms = c[C::WritebackWatchNs as usize] / 1e6,
            write_mb = c[C::WritebackWriteBytes as usize] / 1e6,
            "writeback stages (per frame)"
        );
        tracing::info!(
            frames,
            first_calls = c[C::WritebackFirstCalls as usize],
            failed_calls = c[C::WritebackFailedCalls as usize],
            first_ok_ms = c[C::WritebackFirstOkNs as usize] / 1e6,
            first_failed_ms = c[C::WritebackFirstFailedNs as usize] / 1e6,
            retry_ms = c[C::WritebackRetryNs as usize] / 1e6,
            "mapped write stages (per frame)"
        );
    }
    let mb = |v: f64| v / 1e6;
    let ms = |v: f64| v / 1e3;
    tracing::info!(
        "copies per frame: {:.1} submissions; {:.0} title copies {:.1} MB; {:.0} refreshes {:.1} MB, \
         {:.0} skipped {:.1} MB; {:.0} runs {:.1} ms; {:.0} queries {:.2} ms, moves {:.2} ms, \
         {:.2} MB unmapped; {:.0} page faults; {:.0} watches {:.2} ms, {:.1} MB checked, {:.1} MB \
         unchanged; {:.1} fallbacks; {:.0} guard faults; textures {:.1} MB watched and \
         changed, {:.1} MB hashed, {:.1} MB of them changed",
        c[C::Submissions as usize],
        c[C::Copies as usize],
        mb(c[C::CopyBytes as usize]),
        c[C::Refreshes as usize],
        mb(c[C::RefreshBytes as usize]),
        c[C::Skipped as usize],
        mb(c[C::SkippedBytes as usize]),
        c[C::Runs as usize],
        ms(c[C::RunUs as usize]),
        c[C::Queries as usize],
        ms(c[C::QueryUs as usize]),
        ms(c[C::MoveUs as usize]),
        mb(c[C::Unmapped as usize]),
        faults as f64 / frames,
        c[C::Watches as usize],
        ms(c[C::WatchUs as usize]),
        mb(c[C::Checked as usize]),
        mb(c[C::Unchanged as usize]),
        c[C::Fallbacks as usize],
        c[C::GuardFaults as usize],
        mb(c[C::TexWatched as usize]),
        mb(c[C::TexHashed as usize]),
        mb(c[C::TexChanged as usize]),
    );
    let ranges = RANGES.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut ranges: Vec<_> = ranges.into_iter().flatten().collect();
    ranges.sort_by_key(|&(_, (_, bytes, _, _))| std::cmp::Reverse(bytes));
    for ((src, len), (n, bytes, us, _)) in ranges.into_iter().take(10) {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        unsafe { VirtualQuery(src as *const _, &mut info, size_of_val(&info)) };
        let kind = info.Type;
        tracing::info!(
            "  copied from {src:#x}+{len:#x} (memory {kind:#x}): {:.1}x/frame, {:.2} MB/frame, {:.2} ms/frame",
            n as f64 / frames,
            bytes as f64 / 1e6 / frames,
            us as f64 / 1e3 / frames
        );
    }
}
