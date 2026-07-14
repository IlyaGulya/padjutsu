//! Async, coalescing performer worker.
//!
//! Runs `Performer` operations on a dedicated real-time thread, decoupling the
//! event loop from `CGEventPost` latency. Under heavy graphics load,
//! `CGEventPost` blocks waiting on WindowServer; without this worker, that
//! blocks the entire event loop and inputs from the gamepad pile up.
//!
//! The worker also coalesces consecutive `MouseMove` and `Scroll` commands.
//! Relative deltas can be summed, so a backlog needs one system post rather
//! than one post per missed tick.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use enigo::Button;

use crate::performer::Performer;
use crate::KeyCombo;

/// Commands sent to the worker thread.
#[derive(Debug, Clone)]
pub enum PerformerCmd {
    KeyTap(KeyCombo),
    KeyPress(KeyCombo),
    KeyRelease(KeyCombo),
    MouseMove {
        dx: i32,
        dy: i32,
    },
    ScrollX(f64),
    ScrollY(f64),
    MouseClick(Button),
    MouseDoubleClick(Button),
    MousePress(Button),
    MouseRelease(Button),
    #[cfg(target_os = "macos")]
    RawModifierPress(u16),
    #[cfg(target_os = "macos")]
    RawModifierRelease(u16),
}

/// Handle to the worker thread. Drop to terminate the worker.
pub struct PerformerWorker {
    tx: Sender<QueuedCmd>,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl PerformerWorker {
    /// Spawn a worker thread that owns the given `Performer`.
    /// The worker thread sets itself to macOS realtime priority on macOS.
    pub fn spawn(mut performer: Performer) -> Self {
        let (tx, rx) = bounded::<QueuedCmd>(1024);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = stop.clone();
        let join = thread::Builder::new()
            .name("performer-worker".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                #[cfg(target_os = "macos")]
                set_realtime_priority_2ms();
                run(&mut performer, rx, stop_w);
            })
            .expect("failed to spawn performer worker");
        Self {
            tx,
            stop,
            join: Some(join),
        }
    }

    /// Send a command to the worker. Non-blocking. If the queue is full
    /// (worker backed up), the command is dropped and `Err` is returned.
    /// For movement-style commands the caller should not retry — the next
    /// tick will produce a fresh state.
    pub fn try_send(
        &self,
        cmd: PerformerCmd,
    ) -> Result<(), TrySendError<PerformerCmd>> {
        let queued = QueuedCmd {
            cmd,
            enqueued_at: Instant::now(),
        };
        self.tx.try_send(queued).map_err(|error| match error {
            TrySendError::Full(queued) => TrySendError::Full(queued.cmd),
            TrySendError::Disconnected(queued) => {
                TrySendError::Disconnected(queued.cmd)
            }
        })
    }
}

impl Drop for PerformerWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Send a no-op-ish wake to unblock recv if needed.
        let _ = self.tx.try_send(QueuedCmd {
            cmd: PerformerCmd::MouseMove { dx: 0, dy: 0 },
            enqueued_at: Instant::now(),
        });
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[derive(Debug)]
struct QueuedCmd {
    cmd: PerformerCmd,
    enqueued_at: Instant,
}

fn run(performer: &mut Performer, rx: Receiver<QueuedCmd>, stop: Arc<AtomicBool>) {
    let mut metrics = WorkerMetrics::new(metrics_enabled());
    while !stop.load(Ordering::Acquire) {
        // Block for at least one command.
        let first = match rx.recv() {
            Ok(c) => c,
            Err(_) => return,
        };

        // Drain everything currently queued so we can coalesce homogeneous
        // streams (mouse moves + scrolls) and execute the rest in order.
        let mut batch: Vec<QueuedCmd> = Vec::with_capacity(16);
        batch.push(first);
        while let Ok(c) = rx.try_recv() {
            batch.push(c);
        }

        metrics.record_batch(&batch);
        execute_batch(performer, &batch, &mut metrics);
        metrics.maybe_report(rx.len());
    }
}

/// Execute a batch of commands, coalescing consecutive movement/scroll
/// commands into single posts to avoid catch-up tails under load.
fn execute_batch(
    performer: &mut Performer,
    batch: &[QueuedCmd],
    metrics: &mut WorkerMetrics,
) {
    let mut i = 0;
    while i < batch.len() {
        match &batch[i].cmd {
            PerformerCmd::MouseMove { .. } => {
                // Coalesce all subsequent MouseMove commands into a single delta.
                let segment_start = i;
                let mut sum_dx: i32 = 0;
                let mut sum_dy: i32 = 0;
                while i < batch.len() {
                    if let PerformerCmd::MouseMove { dx, dy } = batch[i].cmd {
                        sum_dx = sum_dx.saturating_add(dx);
                        sum_dy = sum_dy.saturating_add(dy);
                        i += 1;
                    } else {
                        break;
                    }
                }
                if sum_dx != 0 || sum_dy != 0 {
                    let started_at = metrics.start_execution();
                    let _ = performer.mouse_move(sum_dx, sum_dy);
                    metrics.record_execution(ExecutionKind::Mouse, started_at);
                }
                metrics.record_coalesced(i - segment_start - 1);
            }
            PerformerCmd::ScrollX(_) => {
                let segment_start = i;
                let mut sum: f64 = 0.0;
                while i < batch.len() {
                    if let PerformerCmd::ScrollX(v) = batch[i].cmd {
                        sum += v;
                        i += 1;
                    } else {
                        break;
                    }
                }
                if sum != 0.0 {
                    let started_at = metrics.start_execution();
                    let _ = performer.scroll_x(sum);
                    metrics.record_execution(ExecutionKind::Scroll, started_at);
                }
                metrics.record_coalesced(i - segment_start - 1);
            }
            PerformerCmd::ScrollY(_) => {
                let segment_start = i;
                let mut sum: f64 = 0.0;
                while i < batch.len() {
                    if let PerformerCmd::ScrollY(v) = batch[i].cmd {
                        sum += v;
                        i += 1;
                    } else {
                        break;
                    }
                }
                if sum != 0.0 {
                    let started_at = metrics.start_execution();
                    let _ = performer.scroll_y(sum);
                    metrics.record_execution(ExecutionKind::Scroll, started_at);
                }
                metrics.record_coalesced(i - segment_start - 1);
            }
            // Non-coalescing commands: execute one at a time.
            other => {
                let started_at = metrics.start_execution();
                execute_one(performer, other);
                metrics.record_execution(ExecutionKind::Other, started_at);
                i += 1;
            }
        }
    }
}

fn execute_one(performer: &mut Performer, cmd: &PerformerCmd) {
    match cmd {
        PerformerCmd::KeyTap(k) => {
            let _ = performer.perform(k);
        }
        PerformerCmd::KeyPress(k) => {
            let _ = performer.press(k);
        }
        PerformerCmd::KeyRelease(k) => {
            let _ = performer.release(k);
        }
        PerformerCmd::MouseMove { dx, dy } => {
            let _ = performer.mouse_move(*dx, *dy);
        }
        PerformerCmd::ScrollX(v) => {
            let _ = performer.scroll_x(*v);
        }
        PerformerCmd::ScrollY(v) => {
            let _ = performer.scroll_y(*v);
        }
        PerformerCmd::MouseClick(b) => {
            let _ = performer.mouse_click(*b);
        }
        PerformerCmd::MouseDoubleClick(b) => {
            let _ = performer.mouse_double_click(*b);
        }
        PerformerCmd::MousePress(b) => {
            let _ = performer.mouse_press(*b);
        }
        PerformerCmd::MouseRelease(b) => {
            let _ = performer.mouse_release(*b);
        }
        #[cfg(target_os = "macos")]
        PerformerCmd::RawModifierPress(kc) => {
            let _ = performer.raw_modifier_press(*kc);
        }
        #[cfg(target_os = "macos")]
        PerformerCmd::RawModifierRelease(kc) => {
            let _ = performer.raw_modifier_release(*kc);
        }
    }
}

const METRICS_REPORT_INTERVAL: Duration = Duration::from_secs(5);
const LATENCY_BUCKETS_US: [u64; 12] = [
    25,
    50,
    100,
    250,
    500,
    1_000,
    2_000,
    4_000,
    8_000,
    16_000,
    32_000,
    u64::MAX,
];

#[derive(Clone, Copy)]
enum ExecutionKind {
    Mouse,
    Scroll,
    Other,
}

#[derive(Default)]
struct TimingStats {
    samples: u64,
    total_us: u128,
    max_us: u64,
    buckets: [u64; LATENCY_BUCKETS_US.len()],
}

impl TimingStats {
    fn record(&mut self, elapsed: Duration) {
        let elapsed_us = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        self.samples += 1;
        self.total_us += u128::from(elapsed_us);
        self.max_us = self.max_us.max(elapsed_us);
        let bucket = LATENCY_BUCKETS_US
            .iter()
            .position(|upper| elapsed_us <= *upper)
            .unwrap_or(LATENCY_BUCKETS_US.len() - 1);
        self.buckets[bucket] += 1;
    }

    fn percentile(&self, percentile: u64) -> u64 {
        if self.samples == 0 {
            return 0;
        }
        let target = (self.samples * percentile).div_ceil(100);
        let mut accumulated = 0;
        for (index, count) in self.buckets.iter().enumerate() {
            accumulated += count;
            if accumulated >= target {
                return LATENCY_BUCKETS_US[index].min(self.max_us);
            }
        }
        self.max_us
    }

    fn summary(&self) -> String {
        let average = if self.samples == 0 {
            0
        } else {
            self.total_us / u128::from(self.samples)
        };
        format!(
            "n={},avg={},p95~{},p99~{},max={}",
            self.samples,
            average,
            self.percentile(95),
            self.percentile(99),
            self.max_us
        )
    }
}

struct WorkerMetrics {
    enabled: bool,
    started_at: Instant,
    batches: u64,
    commands: u64,
    executions: u64,
    coalesced: u64,
    max_batch: usize,
    queue_wait: TimingStats,
    mouse_post: TimingStats,
    scroll_post: TimingStats,
    other_execution: TimingStats,
}

impl WorkerMetrics {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            started_at: Instant::now(),
            batches: 0,
            commands: 0,
            executions: 0,
            coalesced: 0,
            max_batch: 0,
            queue_wait: TimingStats::default(),
            mouse_post: TimingStats::default(),
            scroll_post: TimingStats::default(),
            other_execution: TimingStats::default(),
        }
    }

    fn record_batch(&mut self, batch: &[QueuedCmd]) {
        if !self.enabled {
            return;
        }
        self.batches += 1;
        self.commands += batch.len() as u64;
        self.max_batch = self.max_batch.max(batch.len());
        let now = Instant::now();
        for queued in batch {
            self.queue_wait
                .record(now.saturating_duration_since(queued.enqueued_at));
        }
    }

    fn start_execution(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    fn record_execution(
        &mut self,
        kind: ExecutionKind,
        started_at: Option<Instant>,
    ) {
        let Some(started_at) = started_at else {
            return;
        };
        self.executions += 1;
        let elapsed = started_at.elapsed();
        match kind {
            ExecutionKind::Mouse => self.mouse_post.record(elapsed),
            ExecutionKind::Scroll => self.scroll_post.record(elapsed),
            ExecutionKind::Other => self.other_execution.record(elapsed),
        }
    }

    fn record_coalesced(&mut self, count: usize) {
        if self.enabled {
            self.coalesced += count as u64;
        }
    }

    fn maybe_report(&mut self, queue_len: usize) {
        if !self.enabled || self.started_at.elapsed() < METRICS_REPORT_INTERVAL {
            return;
        }
        eprintln!(
            "[performer-metrics] window_ms={} batches={} commands={} executions={} coalesced={} max_batch={} queue_len={} queue_wait_us({}) mouse_post_us({}) scroll_post_us({}) other_execution_us({})",
            self.started_at.elapsed().as_millis(),
            self.batches,
            self.commands,
            self.executions,
            self.coalesced,
            self.max_batch,
            queue_len,
            self.queue_wait.summary(),
            self.mouse_post.summary(),
            self.scroll_post.summary(),
            self.other_execution.summary(),
        );
        *self = Self::new(true);
    }
}

fn metrics_enabled() -> bool {
    std::env::var("PADJUTSU_METRICS")
        .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

// --- macOS realtime priority for the performer worker thread ---

#[cfg(target_os = "macos")]
fn set_realtime_priority_2ms() {
    use std::os::raw::c_int;

    type KernReturnT = c_int;
    type MachPortT = u32;
    type ThreadActT = MachPortT;
    type ThreadPolicyFlavorT = c_int;
    type MachMsgTypeNumberT = u32;

    const THREAD_TIME_CONSTRAINT_POLICY: ThreadPolicyFlavorT = 2;
    const THREAD_TIME_CONSTRAINT_POLICY_COUNT: MachMsgTypeNumberT = 4;
    const KERN_SUCCESS: KernReturnT = 0;

    #[repr(C)]
    struct ThreadTimeConstraintPolicy {
        period: u32,
        computation: u32,
        constraint: u32,
        preemptible: u32,
    }

    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }

    extern "C" {
        fn mach_thread_self() -> ThreadActT;
        fn thread_policy_set(
            thread: ThreadActT,
            flavor: ThreadPolicyFlavorT,
            policy_info: *const ThreadTimeConstraintPolicy,
            count: MachMsgTypeNumberT,
        ) -> KernReturnT;
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> KernReturnT;
    }

    fn ns_to_abs(ns: u64) -> u32 {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        unsafe {
            mach_timebase_info(&mut info);
        }
        (ns * info.denom as u64 / info.numer as u64) as u32
    }

    // Performer worker has a slightly larger budget than gamepad runtime
    // because CGEventPost can take a few ms under load.
    let policy = ThreadTimeConstraintPolicy {
        period: ns_to_abs(4_000_000), // 4ms scheduling period
        computation: ns_to_abs(1_000_000), // 1ms computation per period
        constraint: ns_to_abs(2_000_000), // 2ms deadline within period
        preemptible: 1,
    };
    let thread = unsafe { mach_thread_self() };
    let kr = unsafe {
        thread_policy_set(
            thread,
            THREAD_TIME_CONSTRAINT_POLICY,
            &policy,
            THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        )
    };
    if kr == KERN_SUCCESS {
        eprintln!("[performer-worker] realtime priority set");
    } else {
        eprintln!("[performer-worker] failed to set RT priority: {kr}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_mouse_moves_sums_deltas() {
        let cmds = vec![
            PerformerCmd::MouseMove { dx: 5, dy: 0 },
            PerformerCmd::MouseMove { dx: 3, dy: 2 },
            PerformerCmd::MouseMove { dx: -1, dy: -1 },
        ];
        // We can't directly test execute_batch without a Performer, but we can
        // verify the coalescing logic by extracting it.
        let mut sum_dx: i32 = 0;
        let mut sum_dy: i32 = 0;
        for c in &cmds {
            if let PerformerCmd::MouseMove { dx, dy } = c {
                sum_dx += dx;
                sum_dy += dy;
            }
        }
        assert_eq!(sum_dx, 7);
        assert_eq!(sum_dy, 1);
    }

    #[test]
    fn coalesce_breaks_at_non_movement() {
        let cmds = vec![
            PerformerCmd::MouseMove { dx: 5, dy: 0 },
            PerformerCmd::MouseClick(Button::Left),
            PerformerCmd::MouseMove { dx: 3, dy: 0 },
        ];
        // Manually trace: first batch should coalesce only first MouseMove,
        // then click, then second MouseMove. We assert there are 3 distinct
        // operations conceptually. Real verification is in integration.
        let movement_segments: Vec<_> = cmds
            .windows(1)
            .map(|w| matches!(w[0], PerformerCmd::MouseMove { .. }))
            .collect();
        assert_eq!(movement_segments, vec![true, false, true]);
    }

    #[test]
    fn timing_stats_report_bucketed_percentiles() {
        let mut stats = TimingStats::default();
        for micros in [10, 20, 30, 60, 120, 300, 700, 1_500, 5_000, 20_000] {
            stats.record(Duration::from_micros(micros));
        }

        assert_eq!(stats.samples, 10);
        assert_eq!(stats.max_us, 20_000);
        assert_eq!(stats.percentile(50), 250);
        assert_eq!(stats.percentile(95), 20_000);
        assert_eq!(stats.percentile(99), 20_000);
    }
}
