use enigo::{Axis, Button, Enigo, InputResult, NewConError, Settings};
#[cfg(not(target_os = "macos"))]
use enigo::{Coordinate, Direction, Mouse};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use crate::KeyCombo;

#[derive(Debug, Clone, Copy)]
pub(crate) struct MouseMoveObservation {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) posted_dx: i32,
    pub(crate) posted_dy: i32,
    pub(crate) prediction_clamped: bool,
    pub(crate) recovery_warped: bool,
    pub(crate) visual_warp_attempted: bool,
    pub(crate) visual_warped: bool,
    pub(crate) stalled_posts: u8,
    pub(crate) warp_duration: std::time::Duration,
    pub(crate) display_epoch: u64,
}

/// Wrap a CG/Cocoa-using closure in a macOS autorelease pool so any
/// internally-allocated CFData/NSObject autoreleased values are freed
/// at the end of the call. Without a pool, those values accumulate in
/// the absence of a Cocoa main run loop and look like a memory leak.
#[cfg(target_os = "macos")]
#[inline]
fn with_pool<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    objc2::rc::autoreleasepool(|_| f())
}

#[cfg(not(target_os = "macos"))]
#[inline]
fn with_pool<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    f()
}

#[cfg(target_os = "macos")]
mod cg_source {
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use std::cell::RefCell;

    thread_local! {
        static SOURCE: RefCell<Option<CGEventSource>> = const { RefCell::new(None) };
    }

    /// Run a closure with a cached, thread-local CGEventSource.
    /// The source is created once per thread and reused for all events,
    /// avoiding the per-event CFData allocations that would otherwise leak
    /// or churn through the system allocator.
    pub fn with<F, R>(f: F) -> Result<R, &'static str>
    where
        F: FnOnce(&CGEventSource) -> R,
    {
        SOURCE.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                let src =
                    CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
                        .map_err(|_| "failed to create CGEventSource")?;
                *slot = Some(src);
            }
            Ok(f(slot.as_ref().expect("CGEventSource initialized above")))
        })
    }
}

#[cfg(target_os = "macos")]
mod display_configuration {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicU64, Ordering};

    use core_graphics::display::{
        CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback,
        CGDisplayRemoveReconfigurationCallback,
    };

    static EPOCH: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn reconfigured(
        _display: u32,
        flags: u32,
        _user_info: *const c_void,
    ) {
        // CoreGraphics invokes the callback both before and after a change.
        // Publish only completed configurations.
        if flags
            & CGDisplayChangeSummaryFlags::kCGDisplayBeginConfigurationFlag.bits()
            == 0
        {
            EPOCH.fetch_add(1, Ordering::Release);
        }
    }

    pub fn epoch() -> u64 {
        EPOCH.load(Ordering::Acquire)
    }

    pub struct Observer;

    impl Observer {
        pub fn register() -> Option<Self> {
            let result = unsafe {
                CGDisplayRegisterReconfigurationCallback(reconfigured, ptr::null())
            };
            (result == 0).then_some(Self)
        }
    }

    impl Drop for Observer {
        fn drop(&mut self) {
            unsafe {
                CGDisplayRemoveReconfigurationCallback(reconfigured, ptr::null());
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod relative_mouse {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
    use core_graphics::{
        display::{CGDisplay, CGPoint, CGRect},
        event::{
            CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton,
            EventField,
        },
    };
    use enigo::{Button, InputError, InputResult};
    use objc2_app_kit::NSEvent;

    use super::MouseMoveObservation;

    const RESYNC_AFTER_IDLE: Duration = Duration::from_millis(50);
    const DISPLAY_EDGE_EPSILON_PX: f64 = 0.001;
    const DELIVERY_QUEUE_CAPACITY: usize = 1024;
    const NOMINAL_MOUSE_TICK_US: f64 = 8_000.0;
    const MAX_VELOCITY_COMPENSATION: f64 = 1.75;
    const MAX_COMPENSATED_VECTOR_PX: f64 = 44.0;
    const MAX_INTERVAL_SAMPLE_US: f64 = 24_000.0;
    const VELOCITY_EWMA_DIVISOR: f64 = 32.0;

    #[derive(Debug, Clone, Copy)]
    struct MoveCommand {
        dx: i32,
        dy: i32,
        generation: u64,
        enqueued_at: Instant,
    }

    #[derive(Debug, Clone, Copy)]
    pub(super) enum ButtonCommand {
        Click(Button, i64),
        Press(Button),
        Release(Button),
    }

    enum DeliveryMessage {
        Move(MoveCommand),
        Button(ButtonCommand),
    }

    pub(super) struct MouseEventDelivery {
        tx: Sender<DeliveryMessage>,
        stop: Arc<AtomicBool>,
        generation: Arc<AtomicU64>,
        submitted: Arc<AtomicU64>,
        dropped: Arc<AtomicU64>,
        join: Option<thread::JoinHandle<()>>,
    }

    impl MouseEventDelivery {
        pub(super) fn spawn(generation: Arc<AtomicU64>) -> Self {
            let (tx, rx) = bounded(DELIVERY_QUEUE_CAPACITY);
            let stop = Arc::new(AtomicBool::new(false));
            let submitted = Arc::new(AtomicU64::new(0));
            let dropped = Arc::new(AtomicU64::new(0));
            let join = {
                let stop = stop.clone();
                let generation = generation.clone();
                let submitted = submitted.clone();
                let dropped = dropped.clone();
                thread::Builder::new()
                    .name("mouse-event-delivery".into())
                    .stack_size(256 * 1024)
                    .spawn(move || {
                        set_user_interactive_qos();
                        run_delivery(rx, stop, generation, submitted, dropped);
                    })
                    .expect("failed to spawn mouse event delivery worker")
            };
            Self {
                tx,
                stop,
                generation,
                submitted,
                dropped,
                join: Some(join),
            }
        }

        fn submit(&self, dx: i32, dy: i32, generation: u64) {
            self.submitted.fetch_add(1, Ordering::Relaxed);
            let command = MoveCommand {
                dx,
                dy,
                generation,
                enqueued_at: Instant::now(),
            };
            if self.tx.try_send(DeliveryMessage::Move(command)).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }

        fn current_generation(&self) -> u64 {
            self.generation.load(Ordering::Acquire)
        }

        pub(super) fn submit_button(
            &self,
            command: ButtonCommand,
        ) -> InputResult<()> {
            if self
                .tx
                .send_timeout(
                    DeliveryMessage::Button(command),
                    Duration::from_millis(4),
                )
                .is_err()
            {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return Err(InputError::Simulate("mouse delivery queue is full"));
            }
            Ok(())
        }
    }

    fn set_user_interactive_qos() {
        type QosClassT = u32;
        const QOS_CLASS_USER_INTERACTIVE: QosClassT = 0x21;
        unsafe extern "C" {
            fn pthread_set_qos_class_self_np(
                qos_class: QosClassT,
                relative_priority: i32,
            ) -> i32;
        }
        let result =
            unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
        if result != 0 {
            eprintln!("[mouse-event-delivery] failed to set user-interactive QoS: {result}");
        }
    }

    impl Drop for MouseEventDelivery {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct MovePlan {
        destination: CGPoint,
        stalled_posts: u8,
    }

    /// Tracks display bounds and whether WindowServer has applied the previous
    /// move. Every destination starts at the live cursor: elapsed movement is
    /// deliberately dropped while WindowServer is blocked, never replayed as
    /// cursor debt after it recovers.
    #[derive(Debug, Default)]
    pub(super) struct TargetTracker {
        target: Option<CGPoint>,
        last_actual: Option<CGPoint>,
        stalled_posts: u8,
        last_post_at: Option<Instant>,
        display_epoch: Option<u64>,
        display_bounds: Vec<CGRect>,
        display_bounds_epoch: Option<u64>,
        generation: Option<u64>,
    }

    impl TargetTracker {
        fn reset_if_generation_changed(&mut self, generation: u64) {
            if self.generation == Some(generation) {
                return;
            }
            self.target = None;
            self.last_actual = None;
            self.stalled_posts = 0;
            self.last_post_at = None;
            self.generation = Some(generation);
        }

        fn destination(
            &mut self,
            actual: CGPoint,
            dx: i32,
            dy: i32,
            display_epoch: u64,
            now: Instant,
        ) -> MovePlan {
            let idle = self.last_post_at.is_some_and(|last| {
                now.saturating_duration_since(last) >= RESYNC_AFTER_IDLE
            });
            let display_changed = self.display_epoch != Some(display_epoch);
            let pending = self
                .target
                .map(|target| CGPoint::new(target.x - actual.x, target.y - actual.y))
                .unwrap_or_default();
            let direction_reversed =
                axis_reversed(pending.x, dx) || axis_reversed(pending.y, dy);
            let actual_progressed = match self.last_actual {
                Some(previous) => {
                    (previous.x - actual.x).abs() >= 0.5
                        || (previous.y - actual.y).abs() >= 0.5
                }
                None => true,
            };
            let had_pending = pending.x.abs() >= 0.5 || pending.y.abs() >= 0.5;
            if idle
                || display_changed
                || direction_reversed
                || actual_progressed
                || !had_pending
            {
                self.stalled_posts = 0;
            } else {
                self.stalled_posts = self.stalled_posts.saturating_add(1);
            }

            let base = actual;
            let requested = offset_point(actual, dx, dy);
            let destination = self.constrain_to_active_displays(
                base,
                actual,
                requested,
                display_epoch,
            );

            self.target = Some(destination);
            self.last_actual = Some(actual);
            self.last_post_at = Some(now);
            self.display_epoch = Some(display_epoch);
            MovePlan {
                destination,
                stalled_posts: self.stalled_posts,
            }
        }

        fn refresh_display_bounds(&mut self, display_epoch: u64) {
            if self.display_bounds_epoch != Some(display_epoch) {
                self.display_bounds = CGDisplay::active_displays()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|id| CGDisplay::new(id).bounds())
                    .collect();
                self.display_bounds_epoch = Some(display_epoch);
            }
        }

        fn constrain_to_active_displays(
            &mut self,
            base: CGPoint,
            actual: CGPoint,
            requested: CGPoint,
            display_epoch: u64,
        ) -> CGPoint {
            self.refresh_display_bounds(display_epoch);
            if self.display_bounds.is_empty() {
                return requested;
            }

            let current_index = self
                .display_bounds
                .iter()
                .position(|bounds| bounds.contains(&base))
                .or_else(|| {
                    self.display_bounds
                        .iter()
                        .position(|bounds| bounds.contains(&actual))
                });
            let requested_index = self
                .display_bounds
                .iter()
                .position(|bounds| bounds.contains(&requested));
            if let Some(requested_index) = requested_index {
                let connected = current_index.map_or(true, |current_index| {
                    current_index == requested_index
                        || displays_touch(
                            &self.display_bounds[current_index],
                            &self.display_bounds[requested_index],
                        )
                });
                if connected {
                    return requested;
                }
            }

            let current = current_index
                .and_then(|index| self.display_bounds.get(index))
                .or_else(|| {
                    self.display_bounds
                        .iter()
                        .find(|bounds| bounds.contains(&base))
                });
            let Some(bounds) = current else {
                return actual;
            };
            let min_x = bounds.origin.x;
            let min_y = bounds.origin.y;
            let max_x = (bounds.origin.x + bounds.size.width
                - DISPLAY_EDGE_EPSILON_PX)
                .max(min_x);
            let max_y = (bounds.origin.y + bounds.size.height
                - DISPLAY_EDGE_EPSILON_PX)
                .max(min_y);
            CGPoint::new(
                requested.x.clamp(min_x, max_x),
                requested.y.clamp(min_y, max_y),
            )
        }
    }

    fn displays_touch(first: &CGRect, second: &CGRect) -> bool {
        let first_max_x = first.origin.x + first.size.width;
        let first_max_y = first.origin.y + first.size.height;
        let second_max_x = second.origin.x + second.size.width;
        let second_max_y = second.origin.y + second.size.height;
        let y_overlap =
            first.origin.y < second_max_y && second.origin.y < first_max_y;
        let x_overlap =
            first.origin.x < second_max_x && second.origin.x < first_max_x;
        let horizontal_touch = ((first_max_x - second.origin.x).abs()
            <= DISPLAY_EDGE_EPSILON_PX
            || (second_max_x - first.origin.x).abs() <= DISPLAY_EDGE_EPSILON_PX)
            && y_overlap;
        let vertical_touch = ((first_max_y - second.origin.y).abs()
            <= DISPLAY_EDGE_EPSILON_PX
            || (second_max_y - first.origin.y).abs() <= DISPLAY_EDGE_EPSILON_PX)
            && x_overlap;
        horizontal_touch || vertical_touch
    }

    fn axis_reversed(pending: f64, delta: i32) -> bool {
        (pending > 0.0 && delta < 0) || (pending < 0.0 && delta > 0)
    }

    /// Mouse moves must remain coalescible. Under WindowServer load,
    /// non-coalesced events form a downstream queue that can keep moving the
    /// cursor after the stick has already returned to neutral.
    #[inline]
    fn movement_event_flags() -> CGEventFlags {
        CGEventFlags::empty()
    }

    const DELIVERY_LATENCY_BUCKETS_US: [u64; 12] = [
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

    #[derive(Default)]
    struct DeliveryTimingStats {
        samples: u64,
        total_us: u128,
        max_us: u64,
        buckets: [u64; DELIVERY_LATENCY_BUCKETS_US.len()],
    }

    impl DeliveryTimingStats {
        fn record(&mut self, elapsed: Duration) {
            let elapsed_us = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
            self.samples += 1;
            self.total_us += u128::from(elapsed_us);
            self.max_us = self.max_us.max(elapsed_us);
            let bucket = DELIVERY_LATENCY_BUCKETS_US
                .iter()
                .position(|upper| elapsed_us <= *upper)
                .unwrap_or(DELIVERY_LATENCY_BUCKETS_US.len() - 1);
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
                    return DELIVERY_LATENCY_BUCKETS_US[index].min(self.max_us);
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

    struct DeliveryMetrics {
        started_at: Instant,
        post: DeliveryTimingStats,
        queue_age: DeliveryTimingStats,
        coalesced: u64,
        generation_cancelled: u64,
        button_commands: u64,
        button_post: DeliveryTimingStats,
        delivery_step_samples: u64,
        delivery_step_total_px: u64,
        delivery_step_max_px: u64,
        delivery_step_limited: u64,
        stale_cursor_posts: u64,
        stale_cursor_sequence_max: u8,
        velocity_compensation_samples: u64,
        velocity_compensation_total_permille: u64,
        velocity_compensation_max_permille: u64,
        velocity_compensation_limited: u64,
        post_over_4ms: u64,
        post_over_16ms: u64,
        post_over_50ms: u64,
    }

    impl DeliveryMetrics {
        fn new() -> Self {
            Self {
                started_at: Instant::now(),
                post: DeliveryTimingStats::default(),
                queue_age: DeliveryTimingStats::default(),
                coalesced: 0,
                generation_cancelled: 0,
                button_commands: 0,
                button_post: DeliveryTimingStats::default(),
                delivery_step_samples: 0,
                delivery_step_total_px: 0,
                delivery_step_max_px: 0,
                delivery_step_limited: 0,
                stale_cursor_posts: 0,
                stale_cursor_sequence_max: 0,
                velocity_compensation_samples: 0,
                velocity_compensation_total_permille: 0,
                velocity_compensation_max_permille: 0,
                velocity_compensation_limited: 0,
                post_over_4ms: 0,
                post_over_16ms: 0,
                post_over_50ms: 0,
            }
        }

        fn record_post(&mut self, queue_age: Duration, elapsed: Duration) {
            self.post.record(elapsed);
            self.queue_age.record(queue_age);
            self.post_over_4ms += u64::from(elapsed > Duration::from_millis(4));
            self.post_over_16ms += u64::from(elapsed > Duration::from_millis(16));
            self.post_over_50ms += u64::from(elapsed > Duration::from_millis(50));
        }

        fn record_delivery_step(&mut self, observation: MovementPost) {
            self.delivery_step_samples += 1;
            self.delivery_step_total_px += observation.step_axis_px;
            self.delivery_step_max_px =
                self.delivery_step_max_px.max(observation.step_axis_px);
            self.delivery_step_limited += u64::from(observation.step_limited);
            self.stale_cursor_posts += u64::from(observation.stalled_posts > 0);
            self.stale_cursor_sequence_max = self
                .stale_cursor_sequence_max
                .max(observation.stalled_posts);
            self.velocity_compensation_samples += 1;
            self.velocity_compensation_total_permille +=
                observation.velocity_compensation_permille;
            self.velocity_compensation_max_permille = self
                .velocity_compensation_max_permille
                .max(observation.velocity_compensation_permille);
            self.velocity_compensation_limited +=
                u64::from(observation.velocity_compensation_limited);
        }

        fn maybe_report(
            &mut self,
            submitted: &AtomicU64,
            dropped: &AtomicU64,
            queue_len: usize,
            force: bool,
        ) {
            if !force
                && self.started_at.elapsed() < padjutsu_metrics::report_interval()
            {
                return;
            }
            let submitted = submitted.swap(0, Ordering::Relaxed);
            let dropped = dropped.swap(0, Ordering::Relaxed);
            if submitted > 0
                || self.post.samples > 0
                || self.button_commands > 0
                || dropped > 0
            {
                padjutsu_metrics::metric!(
                    "mouse-delivery",
                    "[mouse-delivery-metrics] window_ms={} submitted={} posts={} coalesced={} generation_cancelled={} dropped={} button_commands={} queue_len={} mouse_event_post_us({}) mouse_event_post_over_4ms={} mouse_event_post_over_16ms={} mouse_event_post_over_50ms={} delivery_queue_age_us({}) delivery_step_px(n={},avg={},max={}) display_edge_clamped={} stale_cursor_posts={} stale_cursor_sequence_max={} velocity_compensation_x1000(n={},avg={},max={}) velocity_compensation_limited={} mouse_button_post_us({})",
                    self.started_at.elapsed().as_millis(),
                    submitted,
                    self.post.samples,
                    self.coalesced,
                    self.generation_cancelled,
                    dropped,
                    self.button_commands,
                    queue_len,
                    self.post.summary(),
                    self.post_over_4ms,
                    self.post_over_16ms,
                    self.post_over_50ms,
                    self.queue_age.summary(),
                    self.delivery_step_samples,
                    self.delivery_step_total_px
                        .checked_div(self.delivery_step_samples)
                        .unwrap_or(0),
                    self.delivery_step_max_px,
                    self.delivery_step_limited,
                    self.stale_cursor_posts,
                    self.stale_cursor_sequence_max,
                    self.velocity_compensation_samples,
                    self.velocity_compensation_total_permille
                        .checked_div(self.velocity_compensation_samples)
                        .unwrap_or(0),
                    self.velocity_compensation_max_permille,
                    self.velocity_compensation_limited,
                    self.button_post.summary(),
                );
            }
            *self = Self::new();
        }
    }

    fn drain_latest_move(
        mut latest: MoveCommand,
        rx: &Receiver<DeliveryMessage>,
        pending: &mut Option<DeliveryMessage>,
    ) -> (MoveCommand, u64) {
        let mut coalesced = 0;
        while let Ok(message) = rx.try_recv() {
            match message {
                DeliveryMessage::Move(next) => {
                    latest = next;
                    coalesced += 1;
                }
                button @ DeliveryMessage::Button(_) => {
                    *pending = Some(button);
                    break;
                }
            }
        }
        (latest, coalesced)
    }

    #[derive(Debug, Clone, Copy)]
    struct MovementPost {
        step_axis_px: u64,
        step_limited: bool,
        stalled_posts: u8,
        velocity_compensation_permille: u64,
        velocity_compensation_limited: bool,
    }

    struct VelocityCompensator {
        interval_ewma_us: f64,
    }

    impl Default for VelocityCompensator {
        fn default() -> Self {
            Self {
                interval_ewma_us: NOMINAL_MOUSE_TICK_US,
            }
        }
    }

    impl VelocityCompensator {
        fn reset(&mut self) {
            self.interval_ewma_us = NOMINAL_MOUSE_TICK_US;
        }

        fn compensate(
            &mut self,
            dx: i32,
            dy: i32,
            elapsed: Option<Duration>,
        ) -> (i32, i32, u64, bool) {
            if let Some(elapsed) = elapsed {
                let sample_us = (elapsed.as_micros() as f64)
                    .clamp(NOMINAL_MOUSE_TICK_US, MAX_INTERVAL_SAMPLE_US);
                self.interval_ewma_us +=
                    (sample_us - self.interval_ewma_us) / VELOCITY_EWMA_DIVISOR;
            }
            let requested_scale =
                (self.interval_ewma_us / NOMINAL_MOUSE_TICK_US).max(1.0);
            let scale = requested_scale.min(MAX_VELOCITY_COMPENSATION);
            let mut scaled_x = f64::from(dx) * scale;
            let mut scaled_y = f64::from(dy) * scale;
            let length = scaled_x.hypot(scaled_y);
            let mut limited = requested_scale > MAX_VELOCITY_COMPENSATION;
            if length > MAX_COMPENSATED_VECTOR_PX {
                let vector_scale = MAX_COMPENSATED_VECTOR_PX / length;
                scaled_x *= vector_scale;
                scaled_y *= vector_scale;
                limited = true;
            }
            (
                scaled_x.round() as i32,
                scaled_y.round() as i32,
                (scale * 1_000.0).round() as u64,
                limited,
            )
        }
    }

    fn post_movement_event(
        tracker: &mut TargetTracker,
        command: MoveCommand,
        velocity_compensation_permille: u64,
        velocity_compensation_limited: bool,
    ) -> Result<MovementPost, ()> {
        let pressed = unsafe { NSEvent::pressedMouseButtons() };
        let (event_type, button) = if pressed & 1 > 0 {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if pressed & 2 > 0 {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        let display_epoch = super::display_configuration::epoch();
        let (actual, plan, event) =
            super::cg_source::with(|source| -> Result<_, ()> {
                let actual = CGEvent::new(source.clone())?.location();
                let plan = tracker.destination(
                    actual,
                    command.dx,
                    command.dy,
                    display_epoch,
                    Instant::now(),
                );
                let event = CGEvent::new_mouse_event(
                    source.clone(),
                    event_type,
                    plan.destination,
                    button,
                )?;
                Ok((actual, plan, event))
            })
            .map_err(|_| ())??;
        let (delta_x, delta_y) = movement_delta_fields(0, 0);
        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, delta_x);
        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, delta_y);
        event.set_integer_value_field(
            EventField::EVENT_SOURCE_USER_DATA,
            enigo::EVENT_MARKER as i64,
        );
        event.set_flags(movement_event_flags());
        event.post(CGEventTapLocation::HID);
        Ok(MovementPost {
            step_axis_px: (plan.destination.x - actual.x)
                .abs()
                .max((plan.destination.y - actual.y).abs())
                .round() as u64,
            step_limited: {
                let requested = offset_point(actual, command.dx, command.dy);
                plan.destination.x != requested.x
                    || plan.destination.y != requested.y
            },
            stalled_posts: plan.stalled_posts,
            velocity_compensation_permille,
            velocity_compensation_limited,
        })
    }

    fn run_delivery(
        rx: Receiver<DeliveryMessage>,
        stop: Arc<AtomicBool>,
        generation: Arc<AtomicU64>,
        submitted: Arc<AtomicU64>,
        dropped: Arc<AtomicU64>,
    ) {
        let mut metrics = DeliveryMetrics::new();
        let mut tracker = TargetTracker::default();
        let mut delivery_generation = None;
        let mut last_move_started_at = None;
        let mut velocity_compensator = VelocityCompensator::default();
        let mut pending = None;
        while !stop.load(Ordering::Acquire) {
            let message = match pending.take() {
                Some(message) => message,
                None => match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(message) => message,
                    Err(RecvTimeoutError::Timeout) => {
                        metrics.maybe_report(&submitted, &dropped, rx.len(), false);
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                },
            };
            match message {
                DeliveryMessage::Move(command) => {
                    let (command, coalesced) =
                        drain_latest_move(command, &rx, &mut pending);
                    metrics.coalesced += coalesced;
                    if command.generation != generation.load(Ordering::Acquire) {
                        metrics.generation_cancelled += 1;
                        continue;
                    }
                    if delivery_generation != Some(command.generation) {
                        delivery_generation = Some(command.generation);
                        last_move_started_at = None;
                        velocity_compensator.reset();
                    }
                    tracker.reset_if_generation_changed(command.generation);
                    let started_at = Instant::now();
                    let elapsed = last_move_started_at
                        .replace(started_at)
                        .map(|last| started_at.saturating_duration_since(last));
                    let (dx, dy, compensation_permille, compensation_limited) =
                        velocity_compensator.compensate(
                            command.dx,
                            command.dy,
                            elapsed,
                        );
                    let command = MoveCommand { dx, dy, ..command };
                    let queue_age =
                        started_at.saturating_duration_since(command.enqueued_at);
                    let observation = super::with_pool(|| {
                        post_movement_event(
                            &mut tracker,
                            command,
                            compensation_permille,
                            compensation_limited,
                        )
                    });
                    if let Ok(observation) = observation {
                        metrics.record_delivery_step(observation);
                    }
                    metrics.record_post(queue_age, started_at.elapsed());
                }
                DeliveryMessage::Button(command) => {
                    let started_at = Instant::now();
                    let _ = super::with_pool(|| match command {
                        ButtonCommand::Click(button, count) => {
                            super::native_mouse::click(button, count)
                        }
                        ButtonCommand::Press(button) => {
                            super::native_mouse::press(button)
                        }
                        ButtonCommand::Release(button) => {
                            super::native_mouse::release(button)
                        }
                    });
                    metrics.button_commands += 1;
                    metrics.button_post.record(started_at.elapsed());
                }
            }
            metrics.maybe_report(&submitted, &dropped, rx.len(), false);
        }
        metrics.maybe_report(&submitted, &dropped, rx.len(), true);
    }

    /// Post a relative move while preserving Enigo's macOS semantics.
    ///
    /// Cursor position is read in native Quartz coordinates. This avoids any
    /// dependence on cached display height or AppKit's screen-layout cache,
    /// which can become stale after monitor reconfiguration.
    pub fn post(
        delivery: &MouseEventDelivery,
        dx: i32,
        dy: i32,
    ) -> InputResult<MouseMoveObservation> {
        let display_epoch = super::display_configuration::epoch();
        let generation = delivery.current_generation();
        let point = super::cg_source::with(|source| -> Result<_, ()> {
            let point = CGEvent::new(source.clone())?.location();
            Ok(point)
        })
        .map_err(|_| InputError::Simulate("failed to create mouse source"))?
        .map_err(|_| InputError::Simulate("failed reading cursor position"))?;

        // The worker applies this vector from the cursor position that is live
        // at delivery time. Coalescing therefore drops elapsed motion during a
        // WindowServer stall instead of replaying an absolute-position debt.
        delivery.submit(dx, dy, generation);
        Ok(MouseMoveObservation {
            x: point.x.round() as i32,
            y: point.y.round() as i32,
            posted_dx: dx,
            posted_dy: dy,
            prediction_clamped: false,
            recovery_warped: false,
            visual_warp_attempted: false,
            visual_warped: false,
            stalled_posts: 0,
            warp_duration: Duration::ZERO,
            display_epoch,
        })
    }

    fn offset_point(point: CGPoint, dx: i32, dy: i32) -> CGPoint {
        CGPoint::new(point.x + f64::from(dx), point.y + f64::from(dy))
    }

    pub(super) fn movement_delta_fields(_dx: i32, _dy: i32) -> (i64, i64) {
        // The absolute Quartz destination is the source of truth. Publishing
        // the requested relative delta as well lets WindowServer accumulate
        // invisible motion after the cursor has hit a display edge.
        (0, 0)
    }

    #[cfg(test)]
    mod tests {
        use std::time::{Duration, Instant};

        use core_graphics::display::CGSize;

        use super::*;

        fn tracker_with_bounds(bounds: Vec<CGRect>, epoch: u64) -> TargetTracker {
            TargetTracker {
                display_bounds: bounds,
                display_bounds_epoch: Some(epoch),
                ..TargetTracker::default()
            }
        }

        fn large_test_display() -> CGRect {
            CGRect::new(
                &CGPoint::new(-1_000.0, -1_000.0),
                &CGSize::new(5_000.0, 5_000.0),
            )
        }

        #[test]
        fn movement_events_allow_window_server_coalescing() {
            let flags = movement_event_flags();
            assert!(!flags.contains(CGEventFlags::CGEventFlagNonCoalesced));
            assert_eq!(flags.bits() & 0x2000_0000, 0);
        }

        #[test]
        fn quartz_coordinates_do_not_depend_on_display_height() {
            let destination =
                offset_point(CGPoint::new(757.0, 981.984_375), 12, -20);
            assert_eq!(destination.x, 769.0);
            assert_eq!(destination.y, 961.984_375);
        }

        #[test]
        fn stale_async_delivery_drops_elapsed_motion_instead_of_building_debt() {
            let actual = CGPoint::new(100.0, 200.0);
            let started_at = Instant::now();
            let mut tracker = tracker_with_bounds(vec![large_test_display()], 0);

            let first = tracker.destination(actual, 10, 0, 0, started_at);
            let second = tracker.destination(
                actual,
                10,
                0,
                0,
                started_at + Duration::from_millis(8),
            );
            let fifth = (2..5).fold(second, |_, tick| {
                tracker.destination(
                    actual,
                    10,
                    0,
                    0,
                    started_at + Duration::from_millis(tick * 8),
                )
            });

            assert_eq!(first.destination.x, 110.0);
            assert_eq!(second.destination.x, 110.0);
            assert_eq!(fifth.destination.x, 110.0);
            assert_eq!(fifth.destination.y, actual.y);
            assert_eq!(fifth.stalled_posts, 4);
        }

        #[test]
        fn diagonal_delivery_preserves_the_stick_vector() {
            let actual = CGPoint::new(100.0, 200.0);
            let mut tracker = tracker_with_bounds(vec![large_test_display()], 0);

            let plan = tracker.destination(actual, 22, 11, 0, Instant::now());

            assert_eq!(plan.destination.x - actual.x, 22.0);
            assert_eq!(plan.destination.y - actual.y, 11.0);
        }

        #[test]
        fn velocity_compensation_adapts_gradually_and_ignores_one_long_stall() {
            let mut compensator = VelocityCompensator::default();
            assert_eq!(
                compensator.compensate(20, 10, None),
                (20, 10, 1_000, false)
            );

            let first_delayed = compensator.compensate(
                20,
                10,
                Some(Duration::from_millis(16)),
            );
            assert_eq!(first_delayed, (21, 10, 1_031, false));

            let mut steady = first_delayed;
            for _ in 0..96 {
                steady = compensator.compensate(
                    20,
                    10,
                    Some(Duration::from_millis(16)),
                );
            }
            assert_eq!(steady, (35, 18, 1_750, true));
            assert_eq!(
                compensator.compensate(
                    20,
                    10,
                    Some(Duration::from_millis(200)),
                ),
                steady
            );
        }

        #[test]
        fn velocity_compensation_preserves_non_diagonal_direction() {
            let mut compensator = VelocityCompensator::default();
            for _ in 0..96 {
                let _ = compensator.compensate(
                    22,
                    11,
                    Some(Duration::from_millis(16)),
                );
            }
            let (dx, dy, _, _) = compensator.compensate(
                22,
                11,
                Some(Duration::from_millis(16)),
            );

            assert_eq!((dx, dy), (39, 19));
        }

        #[test]
        fn reversing_direction_discards_unapplied_cursor_lead() {
            let actual = CGPoint::new(100.0, 200.0);
            let started_at = Instant::now();
            let mut tracker = tracker_with_bounds(vec![large_test_display()], 0);
            let _ = tracker.destination(actual, 20, 0, 0, started_at);
            let reversed = tracker.destination(
                actual,
                -5,
                0,
                0,
                started_at + Duration::from_millis(8),
            );

            assert_eq!(reversed.destination.x, 95.0);
            assert_eq!(reversed.stalled_posts, 0);
        }

        #[test]
        fn idle_or_display_change_resynchronizes_with_live_cursor() {
            let started_at = Instant::now();
            let mut tracker = tracker_with_bounds(vec![large_test_display()], 0);
            let _ = tracker.destination(
                CGPoint::new(100.0, 200.0),
                20,
                0,
                0,
                started_at,
            );

            let after_idle = tracker.destination(
                CGPoint::new(500.0, 600.0),
                4,
                0,
                0,
                started_at + Duration::from_millis(100),
            );
            tracker.display_bounds = vec![large_test_display()];
            tracker.display_bounds_epoch = Some(1);
            let after_display_change = tracker.destination(
                CGPoint::new(-300.0, 50.0),
                4,
                0,
                1,
                started_at + Duration::from_millis(108),
            );

            assert_eq!(after_idle.destination.x, 504.0);
            assert_eq!(after_idle.destination.y, 600.0);
            assert_eq!(after_idle.stalled_posts, 0);
            assert_eq!(after_display_change.destination.x, -296.0);
            assert_eq!(after_display_change.destination.y, 50.0);
            assert_eq!(after_display_change.stalled_posts, 0);
        }

        #[test]
        fn stale_delivery_is_observed_without_triggering_synchronous_recovery() {
            let started_at = Instant::now();
            let mut tracker = tracker_with_bounds(vec![large_test_display()], 0);

            let first = tracker.destination(
                CGPoint::new(100.0, 200.0),
                10,
                0,
                0,
                started_at,
            );
            let one_stale = tracker.destination(
                CGPoint::new(100.0, 200.0),
                10,
                0,
                0,
                started_at + Duration::from_millis(8),
            );
            let two_stale = tracker.destination(
                CGPoint::new(100.0, 200.0),
                10,
                0,
                0,
                started_at + Duration::from_millis(16),
            );
            let progressed = tracker.destination(
                CGPoint::new(130.0, 200.0),
                10,
                0,
                0,
                started_at + Duration::from_millis(24),
            );

            assert_eq!(first.stalled_posts, 0);
            assert_eq!(one_stale.stalled_posts, 1);
            assert_eq!(two_stale.stalled_posts, 2);
            assert_eq!(progressed.stalled_posts, 0);
        }

        #[test]
        fn neutral_generation_discards_unapplied_target_before_resume() {
            let actual = CGPoint::new(100.0, 200.0);
            let started_at = Instant::now();
            let mut tracker = tracker_with_bounds(vec![large_test_display()], 0);
            tracker.reset_if_generation_changed(7);
            for tick in 0..10 {
                tracker.destination(
                    actual,
                    20,
                    0,
                    0,
                    started_at + Duration::from_millis(tick * 8),
                );
            }

            tracker.reset_if_generation_changed(8);
            let resumed = tracker.destination(
                actual,
                4,
                0,
                0,
                started_at + Duration::from_millis(81),
            );

            assert_eq!(resumed.destination.x, 104.0);
        }

        #[test]
        fn accumulated_target_stops_at_display_edge_instead_of_entering_gap() {
            let left =
                CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(100.0, 100.0));
            let right =
                CGRect::new(&CGPoint::new(200.0, 0.0), &CGSize::new(100.0, 100.0));
            let started_at = Instant::now();
            let mut tracker = tracker_with_bounds(vec![left, right], 0);
            let edge =
                tracker.destination(CGPoint::new(95.0, 50.0), 20, 0, 0, started_at);

            assert!((edge.destination.x - 99.999).abs() < 0.000_1);
            assert_eq!(edge.destination.y, 50.0);
        }

        #[test]
        fn touching_displays_allow_crossing_but_a_gap_does_not() {
            let left =
                CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(100.0, 100.0));
            let touching =
                CGRect::new(&CGPoint::new(100.0, 0.0), &CGSize::new(100.0, 100.0));
            let separated =
                CGRect::new(&CGPoint::new(101.0, 0.0), &CGSize::new(100.0, 100.0));

            assert!(displays_touch(&left, &touching));
            assert!(!displays_touch(&left, &separated));
        }

        #[test]
        fn delayed_delivery_collapses_to_latest_move_without_crossing_barrier() {
            let (tx, rx) = bounded(8);
            let now = Instant::now();
            let command = |dx, dy| MoveCommand {
                dx,
                dy,
                generation: 7,
                enqueued_at: now,
            };
            tx.send(DeliveryMessage::Move(command(10, 5))).unwrap();
            tx.send(DeliveryMessage::Move(command(20, 10))).unwrap();
            tx.send(DeliveryMessage::Button(ButtonCommand::Click(
                Button::Left,
                1,
            )))
            .unwrap();
            tx.send(DeliveryMessage::Move(command(30, 15))).unwrap();

            let DeliveryMessage::Move(first) = rx.recv().unwrap() else {
                panic!("first delivery message must be movement");
            };
            let mut pending = None;
            let (latest, coalesced) = drain_latest_move(first, &rx, &mut pending);

            assert_eq!((latest.dx, latest.dy), (20, 10));
            assert_eq!(coalesced, 1);
            assert!(matches!(pending, Some(DeliveryMessage::Button(_))));
            assert!(matches!(rx.recv().unwrap(), DeliveryMessage::Move(_)));
        }
    }
}

#[cfg(target_os = "macos")]
mod native_mouse {
    use core_graphics::{
        display::CGPoint,
        event::{
            CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
        },
    };
    use enigo::{Button, InputError, InputResult};

    struct ButtonEventSpec {
        button: CGMouseButton,
        event_type: CGEventType,
        button_number: Option<i64>,
    }

    pub(super) fn event_location(point: CGPoint) -> CGPoint {
        // Quartz already reports global coordinates with the current display
        // layout. Do not convert them through cached AppKit display geometry.
        point
    }

    fn event_spec(button: Button, pressed: bool) -> InputResult<ButtonEventSpec> {
        let (button, down, up, button_number) = match button {
            Button::Left => (
                CGMouseButton::Left,
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
                None,
            ),
            Button::Right => (
                CGMouseButton::Right,
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
                None,
            ),
            Button::Middle => (
                CGMouseButton::Center,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                Some(2),
            ),
            Button::Back => (
                CGMouseButton::Center,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                Some(3),
            ),
            Button::Forward => (
                CGMouseButton::Center,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                Some(4),
            ),
            Button::ScrollUp
            | Button::ScrollDown
            | Button::ScrollLeft
            | Button::ScrollRight => {
                return Err(InputError::InvalidInput(
                    "scroll button is not a pointer button",
                ));
            }
        };
        Ok(ButtonEventSpec {
            button,
            event_type: if pressed { down } else { up },
            button_number,
        })
    }

    fn post(button: Button, pressed: bool, click_count: i64) -> InputResult<()> {
        let spec = event_spec(button, pressed)?;
        let event = super::cg_source::with(|source| -> Result<_, ()> {
            let point = event_location(CGEvent::new(source.clone())?.location());
            CGEvent::new_mouse_event(
                source.clone(),
                spec.event_type,
                point,
                spec.button,
            )
        })
        .map_err(|_| InputError::Simulate("failed to create mouse source"))?
        .map_err(|_| InputError::Simulate("failed creating mouse button event"))?;

        if let Some(button_number) = spec.button_number {
            event.set_integer_value_field(
                EventField::MOUSE_EVENT_BUTTON_NUMBER,
                button_number,
            );
        }
        event.set_integer_value_field(
            EventField::MOUSE_EVENT_CLICK_STATE,
            click_count,
        );
        event.set_integer_value_field(
            EventField::EVENT_SOURCE_USER_DATA,
            enigo::EVENT_MARKER as i64,
        );
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    pub fn click(button: Button, count: i64) -> InputResult<()> {
        for click_count in 1..=count {
            post(button, true, click_count)?;
            post(button, false, click_count)?;
        }
        Ok(())
    }

    pub fn press(button: Button) -> InputResult<()> {
        post(button, true, 1)
    }

    pub fn release(button: Button) -> InputResult<()> {
        post(button, false, 1)
    }
}

#[cfg(target_os = "macos")]
mod raw_modifier {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGEventType};

    /// Device-specific flag bits (from IOKit NX headers).
    const NX_DEVICELCTLKEYMASK: u64 = 0x0000_0001;
    const NX_DEVICERCTLKEYMASK: u64 = 0x0000_2000;
    const NX_DEVICELSHIFTKEYMASK: u64 = 0x0000_0002;
    const NX_DEVICERSHIFTKEYMASK: u64 = 0x0000_0004;
    const NX_DEVICELCMDKEYMASK: u64 = 0x0000_0008;
    const NX_DEVICERCMDKEYMASK: u64 = 0x0000_0010;
    const NX_DEVICELALTKEYMASK: u64 = 0x0000_0020;
    const NX_DEVICERALTKEYMASK: u64 = 0x0000_0040;

    /// macOS virtual keycodes for modifier keys.
    pub const KC_CONTROL: u16 = 0x3B;
    pub const KC_RIGHT_CONTROL: u16 = 0x3E;
    pub const KC_SHIFT: u16 = 0x38;
    pub const KC_RIGHT_SHIFT: u16 = 0x3C;
    pub const KC_COMMAND: u16 = 0x37;
    pub const KC_RIGHT_COMMAND: u16 = 0x36;
    pub const KC_OPTION: u16 = 0x3A;
    pub const KC_RIGHT_OPTION: u16 = 0x3D;

    /// Returns (high-level CGEventFlags mask, device-specific mask) for a modifier keycode.
    fn modifier_flags(keycode: u16) -> Option<(CGEventFlags, u64)> {
        match keycode {
            KC_CONTROL => {
                Some((CGEventFlags::CGEventFlagControl, NX_DEVICELCTLKEYMASK))
            }
            KC_RIGHT_CONTROL => {
                Some((CGEventFlags::CGEventFlagControl, NX_DEVICERCTLKEYMASK))
            }
            KC_SHIFT => {
                Some((CGEventFlags::CGEventFlagShift, NX_DEVICELSHIFTKEYMASK))
            }
            KC_RIGHT_SHIFT => {
                Some((CGEventFlags::CGEventFlagShift, NX_DEVICERSHIFTKEYMASK))
            }
            KC_COMMAND => {
                Some((CGEventFlags::CGEventFlagCommand, NX_DEVICELCMDKEYMASK))
            }
            KC_RIGHT_COMMAND => {
                Some((CGEventFlags::CGEventFlagCommand, NX_DEVICERCMDKEYMASK))
            }
            KC_OPTION => {
                Some((CGEventFlags::CGEventFlagAlternate, NX_DEVICELALTKEYMASK))
            }
            KC_RIGHT_OPTION => {
                Some((CGEventFlags::CGEventFlagAlternate, NX_DEVICERALTKEYMASK))
            }
            _ => None,
        }
    }

    /// Post a FlagsChanged CGEvent, which is what macOS generates for real modifier keypresses.
    pub fn post_flags_changed(keycode: u16, pressed: bool) -> Result<(), String> {
        let (high_flag, dev_flag) = modifier_flags(keycode)
            .ok_or_else(|| format!("keycode 0x{keycode:02x} is not a modifier"))?;

        // Use cached thread-local CGEventSource so we don't allocate a new
        // one per event (CFData leak / churn).
        let event = super::cg_source::with(|source| {
            CGEvent::new_keyboard_event(source.clone(), keycode, pressed)
        })?
        .map_err(|_| "failed to create CGEvent")?;

        // Override event type to FlagsChanged (type 12).
        event.set_type(CGEventType::FlagsChanged);

        // Build the flags bitfield.
        let mut flags = CGEventFlags::CGEventFlagNonCoalesced;
        if pressed {
            flags.insert(high_flag);
            flags.insert(CGEventFlags::from_bits_retain(dev_flag));
        }
        // When releasing, flags should be empty (no modifier held).
        event.set_flags(flags);

        log::info!(
            "[raw_modifier] posting FlagsChanged keycode=0x{keycode:02x} pressed={pressed} flags=0x{:016x}",
            flags.bits()
        );

        // Post at HID level so the event goes through the full macOS input pipeline.
        // Using CombinedSessionState source ensures the global modifier state is updated.
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod smooth_scroll {
    use core_graphics::event::{
        CGEvent, CGEventFlags, CGEventTapLocation, EventField, ScrollEventUnit,
    };

    use enigo::{Axis, InputError, InputResult};
    use log::debug;

    #[derive(Debug, PartialEq, Eq)]
    struct ScrollEventFields {
        fixed_axis_1: i64,
        fixed_axis_2: i64,
        point_axis_1: i64,
        point_axis_2: i64,
        continuous: Option<i64>,
    }

    fn event_fields(
        horizontal: f64,
        vertical: f64,
        continuous: Option<bool>,
    ) -> ScrollEventFields {
        ScrollEventFields {
            fixed_axis_1: (vertical * 65536.0).round() as i64,
            fixed_axis_2: (horizontal * 65536.0).round() as i64,
            point_axis_1: vertical.round() as i64,
            point_axis_2: horizontal.round() as i64,
            continuous: continuous.map(i64::from),
        }
    }

    pub fn post(axis: Axis, value: f64) -> InputResult<()> {
        match axis {
            Axis::Horizontal => post_values(value, 0.0, None, false),
            Axis::Vertical => post_values(0.0, value, None, false),
        }
    }

    pub fn post_trackpad(
        horizontal: f64,
        vertical: f64,
        zoom: bool,
    ) -> InputResult<()> {
        post_values(horizontal, vertical, Some(true), zoom)
    }

    fn trackpad_event_flags(zoom: bool) -> CGEventFlags {
        if zoom {
            CGEventFlags::CGEventFlagCommand
        } else {
            CGEventFlags::empty()
        }
    }

    fn post_values(
        horizontal: f64,
        vertical: f64,
        continuous: Option<bool>,
        zoom: bool,
    ) -> InputResult<()> {
        // Use cached thread-local CGEventSource (see `cg_source` module above)
        // to avoid allocating a fresh source per event.
        let event = super::cg_source::with(|source| {
            CGEvent::new_scroll_event(
                source.clone(),
                ScrollEventUnit::PIXEL,
                2,
                0,
                0,
                0,
            )
        })
        .map_err(|_| InputError::Simulate("failed to create scroll source"))?
        .map_err(|_| InputError::Simulate("failed creating smooth scroll event"))?;

        let fields = event_fields(horizontal, vertical, continuous);

        debug!(
            "[smooth_scroll] horizontal={horizontal:.3} vertical={vertical:.3} continuous_override={continuous:?} fixed1={} fixed2={} point1={} point2={}",
            fields.fixed_axis_1,
            fields.fixed_axis_2,
            fields.point_axis_1,
            fields.point_axis_2
        );

        event
            .set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1, 0);
        event
            .set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2, 0);
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1,
            fields.fixed_axis_1,
        );
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_2,
            fields.fixed_axis_2,
        );
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1,
            fields.point_axis_1,
        );
        event.set_integer_value_field(
            EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2,
            fields.point_axis_2,
        );
        if let Some(continuous) = fields.continuous {
            event.set_integer_value_field(
                EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS,
                continuous,
            );
        }
        event.set_integer_value_field(
            EventField::EVENT_SOURCE_USER_DATA,
            enigo::EVENT_MARKER as i64,
        );
        event.set_flags(trackpad_event_flags(zoom));
        event.post(CGEventTapLocation::HID);
        debug!(
            "[smooth_scroll] posted horizontal={horizontal:.3} vertical={vertical:.3} continuous_override={continuous:?} zoom={zoom}"
        );
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn trackpad_fields_preserve_both_axes_and_mark_event_continuous() {
            assert_eq!(
                event_fields(1.25, -2.5, Some(true)),
                ScrollEventFields {
                    fixed_axis_1: -163_840,
                    fixed_axis_2: 81_920,
                    point_axis_1: -3,
                    point_axis_2: 1,
                    continuous: Some(1),
                }
            );
        }

        #[test]
        fn wheel_scroll_keeps_quartz_pixel_continuous_default() {
            assert_eq!(event_fields(1.0, -2.0, None).continuous, None);
        }

        #[test]
        fn zoom_trackpad_event_carries_command_flag() {
            assert!(trackpad_event_flags(true)
                .contains(CGEventFlags::CGEventFlagCommand));
            assert!(trackpad_event_flags(false).is_empty());
        }
    }
}

pub struct Performer {
    enigo: Enigo,
    mouse_generation: Arc<AtomicU64>,
    #[cfg(target_os = "macos")]
    _display_observer: Option<display_configuration::Observer>,
    #[cfg(target_os = "macos")]
    mouse_delivery: relative_mouse::MouseEventDelivery,
}

// SAFETY: This is safe because we're only accessing Enigo through a Mutex,
// which provides the necessary synchronization. The internal CGEventSource
// is only used on the thread that actually performs the key presses.
unsafe impl Send for Performer {}
unsafe impl Sync for Performer {}

#[cfg(all(test, target_os = "macos"))]
mod display_reconfiguration_regression_tests {
    use core_graphics::display::CGPoint;

    use super::{native_mouse, relative_mouse};

    #[test]
    fn click_uses_live_quartz_point_without_display_height_conversion() {
        let live_point = CGPoint::new(933.5, 868.7);
        let event_point = native_mouse::event_location(live_point);
        assert_eq!(event_point.x, live_point.x);
        assert_eq!(event_point.y, live_point.y);
    }

    #[test]
    fn movement_does_not_publish_hidden_relative_distance_at_screen_edges() {
        assert_eq!(relative_mouse::movement_delta_fields(20, -15), (0, 0));
    }
}

impl Performer {
    /// Create a new performer.
    pub fn new() -> Result<Self, NewConError> {
        let settings = Settings::default();
        let enigo = Enigo::new(&settings)?;
        let mouse_generation = Arc::new(AtomicU64::new(0));
        Ok(Self {
            enigo,
            mouse_generation: mouse_generation.clone(),
            #[cfg(target_os = "macos")]
            _display_observer: display_configuration::Observer::register(),
            #[cfg(target_os = "macos")]
            mouse_delivery: relative_mouse::MouseEventDelivery::spawn(
                mouse_generation,
            ),
        })
    }

    pub(crate) fn mouse_generation(&self) -> Arc<AtomicU64> {
        self.mouse_generation.clone()
    }

    /// Perform key combo.
    /// This will press and release the keys in the key combo.
    pub fn perform(&mut self, key_combo: &KeyCombo) -> InputResult<()> {
        with_pool(|| key_combo.perform(&mut self.enigo))
    }

    /// Press keys.
    pub fn press(&mut self, key_combo: &KeyCombo) -> InputResult<()> {
        with_pool(|| key_combo.press(&mut self.enigo))
    }

    /// Release keys.
    pub fn release(&mut self, key_combo: &KeyCombo) -> InputResult<()> {
        with_pool(|| key_combo.release(&mut self.enigo))
    }

    /// Move mouse.
    #[cfg(target_os = "macos")]
    pub fn mouse_move(&mut self, x: i32, y: i32) -> InputResult<()> {
        self.mouse_move_observed(x, y).map(|_| ())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn mouse_move_observed(
        &mut self,
        x: i32,
        y: i32,
    ) -> InputResult<Option<MouseMoveObservation>> {
        with_pool(|| relative_mouse::post(&self.mouse_delivery, x, y)).map(Some)
    }

    /// Fallback for non-macOS systems.
    #[cfg(not(target_os = "macos"))]
    pub fn mouse_move(&mut self, x: i32, y: i32) -> InputResult<()> {
        with_pool(|| self.enigo.move_mouse(x, y, Coordinate::Rel))
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn mouse_move_observed(
        &mut self,
        x: i32,
        y: i32,
    ) -> InputResult<Option<MouseMoveObservation>> {
        self.mouse_move(x, y).map(|_| None)
    }

    /// Scroll horizontally.
    /// Uses macOS specific smooth scrolling.
    #[cfg(target_os = "macos")]
    pub fn scroll_x(&mut self, value: f64) -> InputResult<()> {
        with_pool(|| smooth_scroll::post(Axis::Horizontal, value))
    }

    /// Scroll vertically.
    /// Uses macOS specific smooth scrolling.
    #[cfg(target_os = "macos")]
    pub fn scroll_y(&mut self, value: f64) -> InputResult<()> {
        with_pool(|| smooth_scroll::post(Axis::Vertical, value))
    }

    /// Post one continuous, two-axis pixel scroll event, matching a trackpad
    /// gesture closely enough for browser canvases to pan diagonally.
    #[cfg(target_os = "macos")]
    pub fn trackpad_scroll(
        &mut self,
        horizontal: f64,
        vertical: f64,
        zoom: bool,
    ) -> InputResult<()> {
        with_pool(|| smooth_scroll::post_trackpad(horizontal, vertical, zoom))
    }

    /// Fallback for non-macOS systems
    #[cfg(not(target_os = "macos"))]
    pub fn scroll_x(&mut self, value: f64) -> InputResult<()> {
        self.enigo.scroll(value.round() as i32, Axis::Horizontal)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn scroll_y(&mut self, value: f64) -> InputResult<()> {
        self.enigo.scroll(value.round() as i32, Axis::Vertical)
    }

    #[cfg(not(target_os = "macos"))]
    pub fn trackpad_scroll(
        &mut self,
        horizontal: f64,
        vertical: f64,
        _zoom: bool,
    ) -> InputResult<()> {
        self.enigo
            .scroll(horizontal.round() as i32, Axis::Horizontal)?;
        self.enigo.scroll(vertical.round() as i32, Axis::Vertical)
    }

    /// Click a mouse button.
    #[cfg(target_os = "macos")]
    pub fn mouse_click(&mut self, button: Button) -> InputResult<()> {
        self.mouse_delivery
            .submit_button(relative_mouse::ButtonCommand::Click(button, 1))
    }

    #[cfg(not(target_os = "macos"))]
    pub fn mouse_click(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| self.enigo.button(button, Direction::Click))
    }

    /// Double-click a mouse button.
    #[cfg(target_os = "macos")]
    pub fn mouse_double_click(&mut self, button: Button) -> InputResult<()> {
        self.mouse_delivery
            .submit_button(relative_mouse::ButtonCommand::Click(button, 2))
    }

    #[cfg(not(target_os = "macos"))]
    pub fn mouse_double_click(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| {
            self.enigo.button(button, Direction::Click)?;
            self.enigo.button(button, Direction::Click)
        })
    }

    /// Press a mouse button (hold down).
    #[cfg(target_os = "macos")]
    pub fn mouse_press(&mut self, button: Button) -> InputResult<()> {
        self.mouse_delivery
            .submit_button(relative_mouse::ButtonCommand::Press(button))
    }

    #[cfg(not(target_os = "macos"))]
    pub fn mouse_press(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| self.enigo.button(button, Direction::Press))
    }

    /// Release a mouse button.
    #[cfg(target_os = "macos")]
    pub fn mouse_release(&mut self, button: Button) -> InputResult<()> {
        self.mouse_delivery
            .submit_button(relative_mouse::ButtonCommand::Release(button))
    }

    #[cfg(not(target_os = "macos"))]
    pub fn mouse_release(&mut self, button: Button) -> InputResult<()> {
        with_pool(|| self.enigo.button(button, Direction::Release))
    }

    /// Send a raw modifier key press via FlagsChanged CGEvent (macOS only).
    /// This is the correct event type for modifier keys — apps like Freeflow
    /// and SuperWhisper that listen for modifier-only keypresses need this.
    #[cfg(target_os = "macos")]
    pub fn raw_modifier_press(&mut self, keycode: u16) -> Result<(), String> {
        with_pool(|| raw_modifier::post_flags_changed(keycode, true))
    }

    /// Send a raw modifier key release via FlagsChanged CGEvent (macOS only).
    #[cfg(target_os = "macos")]
    pub fn raw_modifier_release(&mut self, keycode: u16) -> Result<(), String> {
        with_pool(|| raw_modifier::post_flags_changed(keycode, false))
    }
}
