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
    /// Time in the kernel's write-watch answers, in the view check of the
    /// guard, in protecting; bytes and time of copies of memory neither
    /// watched nor guarded (copied whole).
    KernelUs,
    ViewQueryUs,
    ProtectUs,
    WholeBytes,
    WholeUs,
    /// Snapshots of ranges shaders write (bytes, time); page faults while
    /// preparing submissions and while copying back.
    SnapshotBytes,
    SnapshotUs,
    FaultsPrep,
    FaultsBack,
    /// Pages marked changed because a write landed on another mapping of
    /// them.
    AliasMarks,
    /// Time in the guard handler; protection changes making pages read-only
    /// (calls, pages) and giving write access back (pages).
    HandlerUs,
    ProtectCalls,
    ProtectPages,
    OpenPages,
    /// Tracked pages written (distinct, per frame), and of those the ones
    /// written the frame before too.
    WrittenPages,
    WrittenAgain,
    /// In change checks: time asking the runtime for other mappings, and
    /// waiting for the guard lock.
    AliasUs,
    GuardWaitUs,
    /// Preparing submissions: refresh batches done (copies and change
    /// checks); time in each kind of step (a step that flushes the batch
    /// counts that too); texture uploads for heaps; state preludes.
    FlushUs,
    StepRefreshUs,
    StepCopyUs,
    StepWorkUs,
    StepGpuWriteUs,
    StepTextureUs,
    StepHeapUs,
    StepMemoryUs,
    UploadsUs,
    PreludeUs,
    /// Copying back: bytes of copies compared with their snapshots, and
    /// the time.
    CompareBytes,
    CompareUs,
    /// Copying back pages shaders marked as written through views: pages
    /// compared, bytes written back, the time; pages that kept the title's
    /// bytes (it wrote them, their bytes from before unknown); pages whose
    /// bytes the title's first write saved.
    MarkedPages,
    MarkedBytes,
    MarkedUs,
    MarkedKept,
    SavedPages,
}
const COUNTERS: usize = C::SavedPages as usize + 1;
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

/// Snapshot bytes per host copy (its title start and length).
static SNAPSHOTS: Mutex<Option<HashMap<(u64, u64), u64>>> = Mutex::new(None);

/// A snapshot of `bytes` of the copy of `[start, start + len)`.
pub(crate) fn snapshot(start: u64, len: u64, bytes: u64) {
    *SNAPSHOTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .entry((start, len))
        .or_default() += bytes;
}

/// Our process's working set, bytes.
fn working_set() -> u64 {
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
    c.WorkingSetSize as u64
}

/// Our process's hard page faults so far (read from disk or the page
/// file), from the system's process list: `HardFaultCount` at +0x10 of
/// each entry, the process id at +0x50.
fn hard_faults() -> u64 {
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQuerySystemInformation(class: u32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
    }
    const PROCESSES: u32 = 5;
    let id = std::process::id() as usize;
    let mut buf = vec![0u8; 4 << 20];
    let mut ret = 0;
    // SAFETY: a buffer of that length.
    if unsafe { NtQuerySystemInformation(PROCESSES, buf.as_mut_ptr(), buf.len() as u32, &mut ret) } < 0 {
        return 0;
    }
    let mut at = 0usize;
    loop {
        let entry = &buf[at..];
        let next = u32::from_le_bytes(entry[0..4].try_into().unwrap_or_default()) as usize;
        let pid = usize::from_le_bytes(entry[0x50..0x58].try_into().unwrap_or_default());
        if pid == id {
            return u64::from(u32::from_le_bytes(entry[0x10..0x14].try_into().unwrap_or_default()));
        }
        if next == 0 || at + next + 0x58 > buf.len() {
            return 0;
        }
        at += next;
    }
}

pub(crate) fn page_faults() -> u64 {
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
         changed, {:.1} MB hashed, {:.1} MB of them changed; watch time: kernel {:.1} ms, view queries {:.1} ms, protecting {:.1} ms; copied whole (not tracked) {:.1} MB in {:.1} ms; snapshots {:.1} MB in {:.1} ms; page faults {:.0} preparing, {:.0} copying back; {:.0} tracked pages marked through another mapping; \
         guards: {:.2} ms in the handler, {:.0} protect calls for {:.0} pages, {:.0} pages opened; \
         {:.0} tracked pages written ({:.0} of them the frame before too); change checks: {:.1} ms \
         asking for other mappings, {:.1} ms waiting for the guard lock; preparing: {:.1} ms in \
         refresh batches, steps (each with the batch it flushes): refresh {:.1} ms, copy {:.1} ms, \
         work {:.1} ms, GPU writes {:.1} ms, textures {:.1} ms, heaps {:.1} ms, title memory {:.1} ms; \
         heap texture uploads {:.1} ms, state preludes {:.1} ms; copying back compared {:.1} MB          in {:.1} ms; marked pages: {:.0} compared, {:.1} MB written back in {:.1} ms, {:.0} kept the title's, {:.0} saved; working set {:.0} MB, \
         {:.0} hard faults",
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
        ms(c[C::KernelUs as usize]),
        ms(c[C::ViewQueryUs as usize]),
        ms(c[C::ProtectUs as usize]),
        mb(c[C::WholeBytes as usize]),
        ms(c[C::WholeUs as usize]),
        mb(c[C::SnapshotBytes as usize]),
        ms(c[C::SnapshotUs as usize]),
        c[C::FaultsPrep as usize],
        c[C::FaultsBack as usize],
        c[C::AliasMarks as usize],
        ms(c[C::HandlerUs as usize]),
        c[C::ProtectCalls as usize],
        c[C::ProtectPages as usize],
        c[C::OpenPages as usize],
        c[C::WrittenPages as usize],
        c[C::WrittenAgain as usize],
        ms(c[C::AliasUs as usize]),
        ms(c[C::GuardWaitUs as usize]),
        ms(c[C::FlushUs as usize]),
        ms(c[C::StepRefreshUs as usize]),
        ms(c[C::StepCopyUs as usize]),
        ms(c[C::StepWorkUs as usize]),
        ms(c[C::StepGpuWriteUs as usize]),
        ms(c[C::StepTextureUs as usize]),
        ms(c[C::StepHeapUs as usize]),
        ms(c[C::StepMemoryUs as usize]),
        ms(c[C::UploadsUs as usize]),
        ms(c[C::PreludeUs as usize]),
        mb(c[C::CompareBytes as usize]),
        ms(c[C::CompareUs as usize]),
        c[C::MarkedPages as usize],
        mb(c[C::MarkedBytes as usize]),
        ms(c[C::MarkedUs as usize]),
        c[C::MarkedKept as usize],
        c[C::SavedPages as usize],
        working_set() as f64 / 1e6,
        {
            static HARD: AtomicU64 = AtomicU64::new(0);
            let hard = hard_faults();
            (hard - HARD.swap(hard, Relaxed).min(hard)) as f64 / frames
        },
    );
    let held: Vec<String> = crate::watch::held_times()
        .iter()
        .map(|(line, total, longest)| format!("watch.rs:{line} {:.1} ms (longest {longest:.1})", total / frames))
        .collect();
    tracing::info!("  guard lock held per frame: {}", held.join(", "));
    let mut snapshots: Vec<((u64, u64), u64)> =
        SNAPSHOTS.lock().unwrap_or_else(|e| e.into_inner()).take().into_iter().flatten().collect();
    snapshots.sort_by_key(|&(_, bytes)| std::cmp::Reverse(bytes));
    let snapshots: Vec<String> = snapshots
        .iter()
        .take(6)
        .map(|((start, len), bytes)| format!("{start:#x}+{len:#x} {:.1} MB", *bytes as f64 / 1e6 / frames))
        .collect();
    tracing::info!("  snapshots per frame by copy: {}", snapshots.join(", "));
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
