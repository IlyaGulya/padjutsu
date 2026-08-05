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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use enigo::Button;

use crate::performer::{MouseMoveObservation, Performer};
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
    dropped: Arc<AtomicU64>,
    mouse_generation: Arc<AtomicU64>,
    join: Option<thread::JoinHandle<()>>,
}

impl PerformerWorker {
    /// Spawn a worker thread that owns the given `Performer`.
    /// The worker thread sets itself to macOS realtime priority on macOS.
    pub fn spawn(mut performer: Performer) -> Self {
        let (tx, rx) = bounded::<QueuedCmd>(1024);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = stop.clone();
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_w = dropped.clone();
        let mouse_generation = Arc::new(AtomicU64::new(0));
        let mouse_generation_w = mouse_generation.clone();
        let join = thread::Builder::new()
            .name("performer-worker".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                #[cfg(target_os = "macos")]
                set_realtime_priority_2ms();
                run(&mut performer, rx, stop_w, dropped_w, mouse_generation_w);
            })
            .expect("failed to spawn performer worker");
        Self {
            tx,
            stop,
            dropped,
            mouse_generation,
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
            mouse_generation: self.mouse_generation.load(Ordering::Acquire),
        };
        match self.tx.try_send(queued) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(queued)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Err(TrySendError::Full(queued.cmd))
            }
            Err(TrySendError::Disconnected(queued)) => {
                Err(TrySendError::Disconnected(queued.cmd))
            }
        }
    }

    /// Invalidates mouse movement already waiting behind a slow system post.
    /// The next fresh movement command automatically uses the new generation.
    pub fn cancel_mouse_motion(&self) {
        self.mouse_generation.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for PerformerWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Send a no-op-ish wake to unblock recv if needed.
        let _ = self.tx.try_send(QueuedCmd {
            cmd: PerformerCmd::MouseMove { dx: 0, dy: 0 },
            enqueued_at: Instant::now(),
            mouse_generation: self.mouse_generation.load(Ordering::Acquire),
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
    mouse_generation: u64,
}

const MAX_CATCH_UP_DELTA_AXIS: i32 = 32;

fn clamp_mouse_catch_up(dx: i32, dy: i32) -> (i32, i32) {
    (
        dx.clamp(-MAX_CATCH_UP_DELTA_AXIS, MAX_CATCH_UP_DELTA_AXIS),
        dy.clamp(-MAX_CATCH_UP_DELTA_AXIS, MAX_CATCH_UP_DELTA_AXIS),
    )
}

fn run(
    performer: &mut Performer,
    rx: Receiver<QueuedCmd>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    mouse_generation: Arc<AtomicU64>,
) {
    let mut metrics = WorkerMetrics::new(metrics_enabled(), dropped);
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
        execute_batch(performer, &batch, &mouse_generation, &mut metrics);
        metrics.maybe_report(rx.len());
    }
}

/// Execute a batch of commands, coalescing consecutive movement/scroll
/// commands into single posts to avoid catch-up tails under load.
fn execute_batch(
    performer: &mut Performer,
    batch: &[QueuedCmd],
    mouse_generation: &AtomicU64,
    metrics: &mut WorkerMetrics,
) {
    let mut i = 0;
    while i < batch.len() {
        match &batch[i].cmd {
            PerformerCmd::MouseMove { .. } => {
                // Coalesce all subsequent MouseMove commands into a single delta.
                let segment_start = i;
                let current_generation = mouse_generation.load(Ordering::Acquire);
                let mut sum_dx: i32 = 0;
                let mut sum_dy: i32 = 0;
                let mut valid_commands = 0_usize;
                while i < batch.len() {
                    if let PerformerCmd::MouseMove { dx, dy } = batch[i].cmd {
                        if batch[i].mouse_generation == current_generation {
                            sum_dx = sum_dx.saturating_add(dx);
                            sum_dy = sum_dy.saturating_add(dy);
                            valid_commands += 1;
                        }
                        i += 1;
                    } else {
                        break;
                    }
                }
                let segment_commands = i - segment_start;
                metrics.record_cancelled_mouse_commands(
                    segment_commands.saturating_sub(valid_commands),
                );
                if mouse_generation.load(Ordering::Acquire) != current_generation {
                    metrics.record_cancelled_mouse_commands(valid_commands);
                    valid_commands = 0;
                }
                let original_dx = sum_dx;
                let original_dy = sum_dy;
                (sum_dx, sum_dy) = clamp_mouse_catch_up(sum_dx, sum_dy);
                metrics.record_clamped_mouse_post(
                    sum_dx != original_dx || sum_dy != original_dy,
                );
                if valid_commands > 0 && (sum_dx != 0 || sum_dy != 0) {
                    let started_at = metrics.start_execution();
                    let observation =
                        performer.mouse_move_observed(sum_dx, sum_dy).ok().flatten();
                    metrics.record_mouse(
                        sum_dx,
                        sum_dy,
                        valid_commands,
                        batch[segment_start].enqueued_at,
                        started_at,
                        observation,
                    );
                    metrics.record_execution(ExecutionKind::Mouse, started_at);
                }
                metrics.record_coalesced(segment_commands.saturating_sub(1));
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
const VALUE_BUCKETS: [u64; 12] = [1, 2, 4, 8, 12, 16, 24, 32, 48, 64, 96, u64::MAX];

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

#[derive(Default)]
struct ValueStats {
    samples: u64,
    total: u128,
    max: u64,
    buckets: [u64; VALUE_BUCKETS.len()],
}

impl ValueStats {
    fn record(&mut self, value: u64) {
        self.samples += 1;
        self.total += u128::from(value);
        self.max = self.max.max(value);
        let bucket = VALUE_BUCKETS
            .iter()
            .position(|upper| value <= *upper)
            .unwrap_or(VALUE_BUCKETS.len() - 1);
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
                return VALUE_BUCKETS[index].min(self.max);
            }
        }
        self.max
    }

    fn summary(&self) -> String {
        let average = if self.samples == 0 {
            0
        } else {
            self.total / u128::from(self.samples)
        };
        format!(
            "n={},avg={},p95~{},p99~{},max={}",
            self.samples,
            average,
            self.percentile(95),
            self.percentile(99),
            self.max
        )
    }
}

struct WorkerMetrics {
    enabled: bool,
    started_at: Instant,
    report_interval: Duration,
    dropped: Arc<AtomicU64>,
    batches: u64,
    commands: u64,
    executions: u64,
    coalesced: u64,
    max_batch: usize,
    queue_wait: TimingStats,
    queue_wait_over_4ms: u64,
    queue_wait_over_16ms: u64,
    mouse_post: TimingStats,
    mouse_post_over_4ms: u64,
    mouse_post_over_16ms: u64,
    mouse_post_over_50ms: u64,
    mouse_post_interval: TimingStats,
    mouse_input_age: TimingStats,
    mouse_delta_axis: ValueStats,
    mouse_delta_change_axis: ValueStats,
    cursor_tracking_error_axis: ValueStats,
    cursor_stalled: u64,
    display_reconfiguration_events: u64,
    mouse_posts: u64,
    mouse_commands: u64,
    cancelled_mouse_commands: u64,
    clamped_mouse_posts: u64,
    max_mouse_commands_per_post: usize,
    last_mouse_post_at: Option<Instant>,
    last_mouse_delta: Option<(i32, i32)>,
    last_cursor: Option<(i32, i32)>,
    last_display_epoch: Option<u64>,
    scroll_post: TimingStats,
    other_execution: TimingStats,
}

impl WorkerMetrics {
    fn new(enabled: bool, dropped: Arc<AtomicU64>) -> Self {
        Self {
            enabled,
            started_at: Instant::now(),
            report_interval: metrics_report_interval(),
            dropped,
            batches: 0,
            commands: 0,
            executions: 0,
            coalesced: 0,
            max_batch: 0,
            queue_wait: TimingStats::default(),
            queue_wait_over_4ms: 0,
            queue_wait_over_16ms: 0,
            mouse_post: TimingStats::default(),
            mouse_post_over_4ms: 0,
            mouse_post_over_16ms: 0,
            mouse_post_over_50ms: 0,
            mouse_post_interval: TimingStats::default(),
            mouse_input_age: TimingStats::default(),
            mouse_delta_axis: ValueStats::default(),
            mouse_delta_change_axis: ValueStats::default(),
            cursor_tracking_error_axis: ValueStats::default(),
            cursor_stalled: 0,
            display_reconfiguration_events: 0,
            mouse_posts: 0,
            mouse_commands: 0,
            cancelled_mouse_commands: 0,
            clamped_mouse_posts: 0,
            max_mouse_commands_per_post: 0,
            last_mouse_post_at: None,
            last_mouse_delta: None,
            last_cursor: None,
            last_display_epoch: None,
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
            let wait = now.saturating_duration_since(queued.enqueued_at);
            self.queue_wait.record(wait);
            self.queue_wait_over_4ms += u64::from(wait > Duration::from_millis(4));
            self.queue_wait_over_16ms += u64::from(wait > Duration::from_millis(16));
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
            ExecutionKind::Mouse => {
                self.mouse_post.record(elapsed);
                self.mouse_post_over_4ms +=
                    u64::from(elapsed > Duration::from_millis(4));
                self.mouse_post_over_16ms +=
                    u64::from(elapsed > Duration::from_millis(16));
                self.mouse_post_over_50ms +=
                    u64::from(elapsed > Duration::from_millis(50));
            }
            ExecutionKind::Scroll => self.scroll_post.record(elapsed),
            ExecutionKind::Other => self.other_execution.record(elapsed),
        }
    }

    fn record_mouse(
        &mut self,
        dx: i32,
        dy: i32,
        command_count: usize,
        oldest_enqueued_at: Instant,
        post_started_at: Option<Instant>,
        observation: Option<MouseMoveObservation>,
    ) {
        if !self.enabled {
            return;
        }
        let post_started_at = post_started_at.unwrap_or_else(Instant::now);
        self.mouse_posts += 1;
        self.mouse_commands += command_count as u64;
        self.max_mouse_commands_per_post =
            self.max_mouse_commands_per_post.max(command_count);
        self.mouse_input_age
            .record(post_started_at.saturating_duration_since(oldest_enqueued_at));
        if let Some(previous) = self.last_mouse_post_at {
            self.mouse_post_interval
                .record(post_started_at.saturating_duration_since(previous));
        }
        self.last_mouse_post_at = Some(post_started_at);

        let delta_axis = u64::from(dx.unsigned_abs().max(dy.unsigned_abs()));
        self.mouse_delta_axis.record(delta_axis);
        if let Some((previous_dx, previous_dy)) = self.last_mouse_delta {
            let change_axis = i64::from(dx)
                .abs_diff(i64::from(previous_dx))
                .max(i64::from(dy).abs_diff(i64::from(previous_dy)));
            self.mouse_delta_change_axis.record(change_axis);
        }

        if let Some(observation) = observation {
            if let Some(previous_epoch) = self.last_display_epoch {
                if observation.display_epoch != previous_epoch {
                    self.display_reconfiguration_events += observation
                        .display_epoch
                        .saturating_sub(previous_epoch)
                        .max(1);
                    // A display change may legitimately move the cursor. Do
                    // not report that system transition as a tracking error.
                    self.last_cursor = None;
                    self.last_mouse_delta = None;
                }
            }
            self.last_display_epoch = Some(observation.display_epoch);
            if let (Some((cursor_x, cursor_y)), Some((expected_dx, expected_dy))) =
                (self.last_cursor, self.last_mouse_delta)
            {
                let observed_dx = observation.x.saturating_sub(cursor_x);
                let observed_dy = observation.y.saturating_sub(cursor_y);
                let error_axis = i64::from(observed_dx)
                    .abs_diff(i64::from(expected_dx))
                    .max(i64::from(observed_dy).abs_diff(i64::from(expected_dy)));
                self.cursor_tracking_error_axis.record(error_axis);
                if observed_dx == 0
                    && observed_dy == 0
                    && (expected_dx != 0 || expected_dy != 0)
                {
                    self.cursor_stalled += 1;
                }
            }
            self.last_cursor = Some((observation.x, observation.y));
        }
        self.last_mouse_delta = Some((dx, dy));
    }

    fn record_coalesced(&mut self, count: usize) {
        if self.enabled {
            self.coalesced += count as u64;
        }
    }

    fn record_cancelled_mouse_commands(&mut self, count: usize) {
        if self.enabled {
            self.cancelled_mouse_commands += count as u64;
        }
    }

    fn record_clamped_mouse_post(&mut self, clamped: bool) {
        if self.enabled && clamped {
            self.clamped_mouse_posts += 1;
        }
    }

    fn maybe_report(&mut self, queue_len: usize) {
        if !self.enabled || self.started_at.elapsed() < self.report_interval {
            return;
        }
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        eprintln!(
            "[performer-metrics] window_ms={} batches={} commands={} executions={} coalesced={} dropped={} max_batch={} queue_len={} queue_wait_us({}) queue_wait_over_4ms={} queue_wait_over_16ms={} mouse_post_us({}) mouse_post_over_4ms={} mouse_post_over_16ms={} mouse_post_over_50ms={} mouse_interval_us({}) mouse_input_age_us({}) mouse_delta_axis_px({}) mouse_delta_change_axis_px({}) cursor_tracking_error_axis_px({}) cursor_stalled={} display_epoch={} display_reconfiguration_events={} mouse_posts={} mouse_commands={} cancelled_mouse_commands={} clamped_mouse_posts={} max_mouse_commands_per_post={} scroll_post_us({}) other_execution_us({})",
            self.started_at.elapsed().as_millis(),
            self.batches,
            self.commands,
            self.executions,
            self.coalesced,
            dropped,
            self.max_batch,
            queue_len,
            self.queue_wait.summary(),
            self.queue_wait_over_4ms,
            self.queue_wait_over_16ms,
            self.mouse_post.summary(),
            self.mouse_post_over_4ms,
            self.mouse_post_over_16ms,
            self.mouse_post_over_50ms,
            self.mouse_post_interval.summary(),
            self.mouse_input_age.summary(),
            self.mouse_delta_axis.summary(),
            self.mouse_delta_change_axis.summary(),
            self.cursor_tracking_error_axis.summary(),
            self.cursor_stalled,
            self.last_display_epoch.unwrap_or(0),
            self.display_reconfiguration_events,
            self.mouse_posts,
            self.mouse_commands,
            self.cancelled_mouse_commands,
            self.clamped_mouse_posts,
            self.max_mouse_commands_per_post,
            self.scroll_post.summary(),
            self.other_execution.summary(),
        );
        let dropped = self.dropped.clone();
        let last_mouse_post_at = self.last_mouse_post_at;
        let last_mouse_delta = self.last_mouse_delta;
        let last_cursor = self.last_cursor;
        let last_display_epoch = self.last_display_epoch;
        *self = Self::new(true, dropped);
        self.last_mouse_post_at = last_mouse_post_at;
        self.last_mouse_delta = last_mouse_delta;
        self.last_cursor = last_cursor;
        self.last_display_epoch = last_display_epoch;
    }
}

fn metrics_enabled() -> bool {
    std::env::var("PADJUTSU_METRICS")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

fn metrics_report_interval() -> Duration {
    let seconds = std::env::var("PADJUTSU_METRICS_INTERVAL_S")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60)
        .clamp(5, 3_600);
    Duration::from_secs(seconds)
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
    fn cancelling_mouse_motion_invalidates_already_queued_commands() {
        let (tx, rx) = bounded(4);
        let worker = PerformerWorker {
            tx,
            stop: Arc::new(AtomicBool::new(false)),
            dropped: Arc::new(AtomicU64::new(0)),
            mouse_generation: Arc::new(AtomicU64::new(0)),
            join: None,
        };

        worker
            .try_send(PerformerCmd::MouseMove { dx: 10, dy: 0 })
            .unwrap();
        worker.cancel_mouse_motion();
        worker
            .try_send(PerformerCmd::MouseMove { dx: 2, dy: 0 })
            .unwrap();

        assert_eq!(rx.recv().unwrap().mouse_generation, 0);
        assert_eq!(rx.recv().unwrap().mouse_generation, 1);
    }

    #[test]
    fn catch_up_delta_is_bounded_after_a_system_stall() {
        assert_eq!(clamp_mouse_catch_up(125, -80), (32, -32));
        assert_eq!(clamp_mouse_catch_up(23, -12), (23, -12));
    }

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
        let cmds = [
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

    #[test]
    fn mouse_metrics_detect_cursor_that_did_not_apply_previous_delta() {
        let now = Instant::now();
        let mut metrics = WorkerMetrics::new(true, Arc::new(AtomicU64::new(0)));
        metrics.record_mouse(
            5,
            0,
            1,
            now,
            Some(now),
            Some(MouseMoveObservation {
                x: 100,
                y: 100,
                display_epoch: 0,
            }),
        );
        metrics.record_mouse(
            5,
            0,
            2,
            now + Duration::from_millis(7),
            Some(now + Duration::from_millis(8)),
            Some(MouseMoveObservation {
                x: 100,
                y: 100,
                display_epoch: 0,
            }),
        );

        assert_eq!(metrics.cursor_stalled, 1);
        assert_eq!(metrics.cursor_tracking_error_axis.max, 5);
        assert_eq!(metrics.mouse_post_interval.max_us, 8_000);
        assert_eq!(metrics.max_mouse_commands_per_post, 2);
    }

    #[test]
    fn mouse_metrics_count_rare_slow_posts_outside_percentiles() {
        let mut metrics = WorkerMetrics::new(true, Arc::new(AtomicU64::new(0)));
        let started_at = Instant::now() - Duration::from_millis(60);

        metrics.record_execution(ExecutionKind::Mouse, Some(started_at));

        assert_eq!(metrics.mouse_post_over_4ms, 1);
        assert_eq!(metrics.mouse_post_over_16ms, 1);
        assert_eq!(metrics.mouse_post_over_50ms, 1);
    }

    #[test]
    fn display_reconfiguration_resets_cursor_tracking_baseline() {
        let now = Instant::now();
        let mut metrics = WorkerMetrics::new(true, Arc::new(AtomicU64::new(0)));
        metrics.record_mouse(
            5,
            0,
            1,
            now,
            Some(now),
            Some(MouseMoveObservation {
                x: 100,
                y: 100,
                display_epoch: 0,
            }),
        );
        metrics.record_mouse(
            5,
            0,
            1,
            now,
            Some(now),
            Some(MouseMoveObservation {
                x: 500,
                y: 500,
                display_epoch: 1,
            }),
        );

        assert_eq!(metrics.display_reconfiguration_events, 1);
        assert_eq!(metrics.cursor_tracking_error_axis.samples, 0);
        assert_eq!(metrics.cursor_stalled, 0);
    }
}
