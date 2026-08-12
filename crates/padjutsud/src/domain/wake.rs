use std::time::{Duration, Instant};

use colored::Colorize;

use crate::app::Padjutsu;
use crate::domain::{DomainEvent, TimerEvent};
use crate::domain::transition::WakeTransition;
use crate::print_debug;

pub struct WakeState {
    pub need_reschedule: bool,
    pub ticking_enabled: bool,
    pub fast_mode: bool,
    pub fast_until: Instant,
    pub next_tick_due: Option<Instant>,
    metrics: WakeMetrics,
}

pub struct WakePlan {
    pub repeat_due: Option<Instant>,
    pub button_repeat_due: Option<Instant>,
    pub next_due: Option<Instant>,
}

impl WakeState {
    pub fn new(now: Instant) -> Self {
        Self {
            need_reschedule: true,
            ticking_enabled: false,
            fast_mode: false,
            fast_until: now,
            next_tick_due: None,
            metrics: WakeMetrics::new(),
        }
    }

    pub fn record_timer_wake(&mut self, now: Instant) {
        self.metrics.record(now, self.next_tick_due);
    }

    pub fn record_axis_snapshot_corrections(&mut self, count: u64) {
        self.metrics.axis_snapshot_corrections += count;
    }
}

const WAKE_LATENCY_BUCKETS_US: [u64; 12] = [
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

struct WakeMetrics {
    enabled: bool,
    started_at: Instant,
    report_interval: Duration,
    wakes: u64,
    tick_wakes: u64,
    early_wakes: u64,
    lateness_samples: u64,
    lateness_total_us: u128,
    lateness_max_us: u64,
    lateness_buckets: [u64; WAKE_LATENCY_BUCKETS_US.len()],
    over_1ms: u64,
    over_4ms: u64,
    over_8ms: u64,
    over_16ms: u64,
    axis_snapshot_corrections: u64,
}

impl WakeMetrics {
    fn new() -> Self {
        Self {
            enabled: metrics_enabled(),
            started_at: Instant::now(),
            report_interval: metrics_report_interval(),
            wakes: 0,
            tick_wakes: 0,
            early_wakes: 0,
            lateness_samples: 0,
            lateness_total_us: 0,
            lateness_max_us: 0,
            lateness_buckets: [0; WAKE_LATENCY_BUCKETS_US.len()],
            over_1ms: 0,
            over_4ms: 0,
            over_8ms: 0,
            over_16ms: 0,
            axis_snapshot_corrections: 0,
        }
    }

    fn record(&mut self, now: Instant, tick_due: Option<Instant>) {
        if !self.enabled {
            return;
        }
        self.wakes += 1;
        if let Some(tick_due) = tick_due {
            if now >= tick_due {
                self.tick_wakes += 1;
                let lateness_us =
                    now.saturating_duration_since(tick_due)
                        .as_micros()
                        .min(u128::from(u64::MAX)) as u64;
                self.lateness_samples += 1;
                self.lateness_total_us += u128::from(lateness_us);
                self.lateness_max_us = self.lateness_max_us.max(lateness_us);
                let bucket = WAKE_LATENCY_BUCKETS_US
                    .iter()
                    .position(|upper| lateness_us <= *upper)
                    .unwrap_or(WAKE_LATENCY_BUCKETS_US.len() - 1);
                self.lateness_buckets[bucket] += 1;
                self.over_1ms += u64::from(lateness_us > 1_000);
                self.over_4ms += u64::from(lateness_us > 4_000);
                self.over_8ms += u64::from(lateness_us > 8_000);
                self.over_16ms += u64::from(lateness_us > 16_000);
            } else {
                self.early_wakes += 1;
            }
        }
        self.maybe_report();
    }

    fn percentile(&self, percentile: u64) -> u64 {
        if self.lateness_samples == 0 {
            return 0;
        }
        let target = (self.lateness_samples * percentile).div_ceil(100);
        let mut accumulated = 0;
        for (index, count) in self.lateness_buckets.iter().enumerate() {
            accumulated += count;
            if accumulated >= target {
                return WAKE_LATENCY_BUCKETS_US[index].min(self.lateness_max_us);
            }
        }
        self.lateness_max_us
    }

    fn maybe_report(&mut self) {
        if self.started_at.elapsed() < self.report_interval {
            return;
        }
        let average = if self.lateness_samples == 0 {
            0
        } else {
            self.lateness_total_us / u128::from(self.lateness_samples)
        };
        padjutsu_metrics::metric!(
            "wake",
            "[wake-metrics] window_ms={} wakes={} tick_wakes={} early_wakes={} lateness_us(n={},avg={},p95~{},p99~{},max={}) over_1ms={} over_4ms={} over_8ms={} over_16ms={} axis_snapshot_corrections={}",
            self.started_at.elapsed().as_millis(),
            self.wakes,
            self.tick_wakes,
            self.early_wakes,
            self.lateness_samples,
            average,
            self.percentile(95),
            self.percentile(99),
            self.lateness_max_us,
            self.over_1ms,
            self.over_4ms,
            self.over_8ms,
            self.over_16ms,
            self.axis_snapshot_corrections,
        );
        *self = Self::new();
    }
}

fn metrics_enabled() -> bool {
    padjutsu_metrics::enabled()
}

fn metrics_report_interval() -> Duration {
    padjutsu_metrics::report_interval()
}

pub fn apply_wake_intents(wake_state: &mut WakeState, intents: Vec<WakeTransition>) {
    for intent in intents {
        match intent {
            WakeTransition::Reschedule => {
                wake_state.need_reschedule = true;
            }
            WakeTransition::EnableFastModeUntil(until) => {
                wake_state.fast_mode = true;
                wake_state.fast_until = until;
            }
            WakeTransition::DisableFastMode => {
                wake_state.fast_mode = false;
            }
        }
    }
}

pub fn reschedule_wake(
    padjutsu: &Padjutsu,
    wake_state: &mut WakeState,
    idle_period: Duration,
    fast_period: Duration,
) -> WakePlan {
    let now = Instant::now();
    if padjutsu.needs_tick() {
        let was_ticking_enabled = wake_state.ticking_enabled;
        let previous_tick_due = wake_state.next_tick_due;
        if !wake_state.ticking_enabled {
            // The previous active movement may have ended long ago. Do not feed
            // that idle gap into the first resumed movement calculation.
            padjutsu.reset_tick_clock();
            wake_state.fast_mode = padjutsu.wants_fast_tick();
            if wake_state.fast_mode {
                wake_state.fast_until = now + Duration::from_millis(250);
            }
        }
        let period = if wake_state.fast_mode {
            if padjutsu.wants_continuous_tick_mode() {
                Duration::from_millis(padjutsu.continuous_tick_ms().unwrap_or(4))
            } else {
                fast_period
            }
        } else {
            idle_period
        };
        let desired_tick_due = now + period;
        wake_state.next_tick_due = match previous_tick_due {
            Some(existing_due) if was_ticking_enabled && existing_due > now => {
                Some(core::cmp::min(existing_due, desired_tick_due))
            }
            _ => Some(desired_tick_due),
        };
        wake_state.ticking_enabled = true;
        let next_tick_in = wake_state
            .next_tick_due
            .map(|due| due.saturating_duration_since(now).as_millis())
            .unwrap_or_default();
        print_debug!(
            "wake reschedule: ticking_enabled=true fast_mode={} next_tick_in_ms={}",
            wake_state.fast_mode,
            next_tick_in
        );
    } else {
        wake_state.next_tick_due = None;
        wake_state.ticking_enabled = false;
        wake_state.fast_mode = false;
        print_debug!("wake reschedule: ticking disabled");
    }

    let repeat_due = padjutsu.next_repeat_due();
    let button_repeat_due = padjutsu.next_button_repeat_due();
    let mut next_due = wake_state.next_tick_due;
    for candidate in [repeat_due, button_repeat_due] {
        next_due = match (next_due, candidate) {
            (Some(a), Some(b)) => Some(core::cmp::min(a, b)),
            (Some(a), None) => Some(a),
            (None, b) => b,
        };
    }

    WakePlan {
        repeat_due,
        button_repeat_due,
        next_due,
    }
}

pub fn has_overdue_work(
    padjutsu: &Padjutsu,
    wake_state: &WakeState,
    now: Instant,
) -> bool {
    wake_state.next_tick_due.is_some_and(|due| due <= now)
        || padjutsu.next_repeat_due().is_some_and(|due| due <= now)
        || padjutsu
            .next_button_repeat_due()
            .is_some_and(|due| due <= now)
}

pub fn overdue_wake_event(
    padjutsu: &Padjutsu,
    wake_state: &WakeState,
    now: Instant,
) -> Option<DomainEvent> {
    let tick_due = wake_state.next_tick_due.is_some_and(|due| due <= now);
    let stick_repeat_due = padjutsu.next_repeat_due().is_some_and(|due| due <= now);
    let button_repeat_due = padjutsu
        .next_button_repeat_due()
        .is_some_and(|due| due <= now);

    if has_overdue_work(padjutsu, wake_state, now) {
        print_debug!(
            "processing overdue wake: tick_due={} stick_repeat_due={} button_repeat_due={}",
            tick_due,
            stick_repeat_due,
            button_repeat_due
        );
        Some(DomainEvent::Timer(TimerEvent::Wake))
    } else {
        None
    }
}

#[cfg(test)]
mod metrics_tests {
    use super::*;

    #[test]
    fn wake_metrics_classify_deadline_lateness() {
        let now = Instant::now();
        let mut metrics = WakeMetrics::new();
        metrics.enabled = true;
        metrics.report_interval = Duration::from_secs(3_600);
        metrics.record(now, Some(now - Duration::from_millis(9)));

        assert_eq!(metrics.tick_wakes, 1);
        assert_eq!(metrics.over_1ms, 1);
        assert_eq!(metrics.over_4ms, 1);
        assert_eq!(metrics.over_8ms, 1);
        assert_eq!(metrics.over_16ms, 0);
        assert_eq!(metrics.lateness_max_us, 9_000);
    }
}
