use ahash::AHashMap;
use colored::Colorize;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::time::Instant;
use padjutsu_control::Key;
use padjutsu_gamepad::ControllerId;
use padjutsu_workspace::{Axis as ProfileAxis, StickSide};

use crate::app::Effect;
use crate::print_debug;

use super::util::{side_index};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Default)]
pub(crate) struct StickProcessor {
    pub(super) controllers: AHashMap<ControllerId, ControllerRepeatState>,
    pub(super) generation: u64,
    pub(super) regs: Vec<RepeatReg>,
    schedule: BinaryHeap<SchedEntry>,
    seq_counter: u64,
    pub(super) last_tick_at: Option<Instant>,
    pub(super) perf: TickPerfStats,
}

#[derive(Default)]
pub(super) struct TickPerfStats {
    pub(super) samples: u64,
    pub(super) expected_tick_us: u64,
    pub(super) tick_interval_us: PerfHistogram,
    pub(super) tick_execution_us: PerfHistogram,
    pub(super) gap_over_1_5x: u64,
    pub(super) gap_over_2x: u64,
    pub(super) gap_over_4x: u64,
    pub(super) missed_periods: u64,
    pub(super) mouse_mode_ticks: u64,
    pub(super) mouse_move_events: u64,
    pub(super) mouse_zero_move_ticks: u64,
    pub(super) mouse_distance_total: f64,
    pub(super) mouse_chunk_max: u64,
    pub(super) mouse_chunk_over_8: u64,
    pub(super) mouse_chunk_over_16: u64,
    pub(super) mouse_chunk_over_32: u64,
    pub(super) scroll_events: u64,
    pub(super) trackpad_scroll_events: u64,
    pub(super) last_report_at: Option<Instant>,
}

const PERF_BUCKETS: [u64; 13] = [
    25,
    50,
    100,
    250,
    500,
    1_000,
    2_000,
    4_000,
    8_000,
    12_000,
    16_000,
    32_000,
    u64::MAX,
];

#[derive(Default)]
pub(super) struct PerfHistogram {
    samples: u64,
    total: u128,
    max: u64,
    buckets: [u64; PERF_BUCKETS.len()],
}

impl PerfHistogram {
    pub(super) fn record(&mut self, value: u64) {
        self.samples += 1;
        self.total += u128::from(value);
        self.max = self.max.max(value);
        let bucket = PERF_BUCKETS
            .iter()
            .position(|upper| value <= *upper)
            .unwrap_or(PERF_BUCKETS.len() - 1);
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
                return PERF_BUCKETS[index].min(self.max);
            }
        }
        self.max
    }

    pub(super) fn summary(&self) -> String {
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

#[derive(Default)]
pub(super) struct MousePerfFrame {
    pub(super) mode_active: bool,
    pub(super) move_events: u64,
    pub(super) distance_total: f64,
    pub(super) chunk_max: u64,
    pub(super) chunk_over_8: u64,
    pub(super) chunk_over_16: u64,
    pub(super) chunk_over_32: u64,
}

#[derive(Default)]
pub(super) struct ControllerRepeatState {
    pub(super) sides: [SideRepeatState; 2],
}

#[derive(Default)]
pub(super) struct SideRepeatState {
    pub(super) mouse_filtered: (f32, f32),
    pub(super) mouse_accum: (f32, f32),
    pub(super) scroll_filtered: (f32, f32),
    pub(super) scroll_accum: (f32, f32),
    /// Cumulative absolute scroll per axis since last lock reset (for axis_lock).
    pub(super) scroll_axis_cum: (f32, f32),
    /// Accumulated idle time in deadzone (seconds) for axis_lock reset hysteresis.
    pub(super) scroll_idle_s: f32,
    pub(super) arrows: [Option<RepeatTaskState>; 4],
    pub(super) volume: [Option<RepeatTaskState>; 4],
    pub(super) brightness: [Option<RepeatTaskState>; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepeatKind {
    Arrow(Direction),
    Volume { axis: ProfileAxis, positive: bool },
    Brightness { axis: ProfileAxis, positive: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct RepeatTaskId {
    pub(super) controller: ControllerId,
    pub(super) side: StickSide,
    pub(super) kind: RepeatKind,
}

pub(super) struct RepeatTaskState {
    pub(super) key: Key,
    pub(super) fire_on_activate: bool,
    pub(super) initial_delay_ms: u64,
    pub(super) interval_ms: u64,
    pub(super) last_fire: std::time::Instant,
    pub(super) delay_done: bool,
    pub(super) last_seen_generation: u64,
    pub(super) seq: u64,
}

pub(super) struct RepeatReg {
    pub(super) id: RepeatTaskId,
    pub(super) key: Key,
    pub(super) fire_on_activate: bool,
    pub(super) initial_delay_ms: u64,
    pub(super) interval_ms: u64,
}

impl StickProcessor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset_tick_clock(&mut self) {
        self.last_tick_at = None;
    }

    pub(super) fn dir_index(dir: Direction) -> usize {
        match dir {
            Direction::Up => 0,
            Direction::Down => 1,
            Direction::Left => 2,
            Direction::Right => 3,
        }
    }

    pub(super) fn step_slot_index(axis: ProfileAxis, positive: bool) -> usize {
        match (axis, positive) {
            (ProfileAxis::X, false) => 0,
            (ProfileAxis::X, true) => 1,
            (ProfileAxis::Y, false) => 2,
            (ProfileAxis::Y, true) => 3,
        }
    }

    pub fn release_all_for(&mut self, id: ControllerId) {
        self.controllers.remove(&id);
    }

    pub fn release_all_arrows(&mut self) {
        for (_cid, state) in self.controllers.iter_mut() {
            for s in 0..2 {
                for slot in state.sides[s].arrows.iter_mut() {
                    *slot = None;
                }
            }
        }
    }

    pub fn on_app_change(&mut self) {
        self.release_all_arrows();
        for (_cid, state) in self.controllers.iter_mut() {
            for s in 0..2 {
                state.sides[s].scroll_accum = (0.0, 0.0);
            }
        }
    }

    pub(super) fn repeater_register(
        &mut self,
        reg: RepeatReg,
        now: std::time::Instant,
    ) -> Option<Effect> {
        let cid = reg.id.controller;
        let side_idx = side_index(&reg.id.side);
        // Precompute a fresh seq; consume it only when needed.
        let seq_new = self.next_seq();

        let mut action: Option<Effect> = None;
        let mut schedule_next: Option<(RepeatTaskId, u64, std::time::Instant)> =
            None;

        {
            let ctrl = self.controllers.entry(cid).or_default();
            let side = &mut ctrl.sides[side_idx];
            let slot: &mut Option<RepeatTaskState> = match reg.id.kind {
                RepeatKind::Arrow(dir) => {
                    let idx = Self::dir_index(dir);
                    &mut side.arrows[idx]
                }
                RepeatKind::Volume { axis, positive } => {
                    let idx = Self::step_slot_index(axis, positive);
                    &mut side.volume[idx]
                }
                RepeatKind::Brightness { axis, positive } => {
                    let idx = Self::step_slot_index(axis, positive);
                    &mut side.brightness[idx]
                }
            };

            match slot {
                Some(st) => {
                    let changed = st.key != reg.key
                        || st.interval_ms != reg.interval_ms
                        || st.initial_delay_ms != reg.initial_delay_ms
                        || st.fire_on_activate != reg.fire_on_activate;
                    st.key = reg.key;
                    st.interval_ms = reg.interval_ms;
                    st.initial_delay_ms = reg.initial_delay_ms;
                    st.fire_on_activate = reg.fire_on_activate;
                    st.last_seen_generation = self.generation;

                    if changed {
                        st.seq = seq_new;
                        let due_ms = if st.delay_done {
                            st.interval_ms
                        } else {
                            st.initial_delay_ms
                        };
                        if due_ms > 0 {
                            schedule_next = Some((
                                reg.id,
                                st.seq,
                                now + std::time::Duration::from_millis(due_ms),
                            ));
                        }
                    }
                }
                None => {
                    let delay_done = reg.initial_delay_ms == 0;
                    let st = RepeatTaskState {
                        key: reg.key,
                        fire_on_activate: reg.fire_on_activate,
                        initial_delay_ms: reg.initial_delay_ms,
                        interval_ms: reg.interval_ms,
                        last_fire: now,
                        delay_done,
                        last_seen_generation: self.generation,
                        seq: seq_new,
                    };
                    *slot = Some(st);
                    if reg.fire_on_activate {
                        action = Some(Effect::KeyTap(
                            padjutsu_control::KeyCombo::from_key(reg.key),
                        ));
                    }
                    let due_ms = if delay_done {
                        reg.interval_ms
                    } else {
                        reg.initial_delay_ms
                    };
                    if due_ms > 0 {
                        schedule_next = Some((
                            reg.id,
                            seq_new,
                            now + std::time::Duration::from_millis(due_ms),
                        ));
                    }
                }
            }
        }

        if let Some((id, seq, due)) = schedule_next {
            print_debug!(
                "stick repeat schedule: controller={} side={:?} kind={:?} seq={seq} due_in_ms={}",
                id.controller,
                id.side,
                id.kind,
                due.saturating_duration_since(now).as_millis()
            );
            self.push_due(id, seq, due);
        }

        action
    }

    pub fn next_repeat_due(&mut self) -> Option<Instant> {
        while let Some(entry) = self.schedule.peek() {
            if self.entry_is_stale(entry) {
                let _ = self.schedule.pop();
                continue;
            }
            return Some(entry.due);
        }
        None
    }

    pub fn process_due_repeats(
        &mut self,
        now: Instant,
        sink: &mut impl FnMut(Effect),
    ) {
        loop {
            let entry = match self.schedule.peek() {
                Some(top) if self.entry_is_stale(top) => {
                    let _ = self.schedule.pop();
                    continue;
                }
                Some(top) if top.due <= now => self.schedule.pop().unwrap(),
                _ => break,
            };

            let mut schedule_next: Option<(RepeatTaskId, u64, Instant)> = None;
            {
                if let Some(slot) = self.slot_for_mut(&entry.id) {
                    if let Some(st) = slot.as_mut() {
                        if st.seq == entry.seq {
                            print_debug!(
                                "stick repeat fire: controller={} side={:?} kind={:?} seq={} key={:?}",
                                entry.id.controller,
                                entry.id.side,
                                entry.id.kind,
                                entry.seq,
                                st.key
                            );
                            (sink)(Effect::KeyTap(
                                padjutsu_control::KeyCombo::from_key(st.key),
                            ));
                            st.last_fire = now;
                            st.delay_done = true;
                            let next_due = now
                                + std::time::Duration::from_millis(st.interval_ms);
                            schedule_next = Some((entry.id, st.seq, next_due));
                        }
                    }
                }
            }
            if let Some((id, seq, due)) = schedule_next {
                print_debug!(
                    "stick repeat reschedule: controller={} side={:?} kind={:?} seq={seq} due_in_ms={}",
                    id.controller,
                    id.side,
                    id.kind,
                    due.saturating_duration_since(now).as_millis()
                );
                self.push_due(id, seq, due);
            }
        }
    }

    pub(super) fn repeater_cleanup_inactive(&mut self) {
        let gen = self.generation;
        for (cid, ctrl) in self.controllers.iter_mut() {
            for side in ctrl.sides.iter_mut() {
                for slot in side.arrows.iter_mut() {
                    if let Some(st) = slot.as_ref() {
                        if st.last_seen_generation != gen {
                            print_debug!(
                                "stick repeat cleanup: controller={cid} repeat_slot=Arrow"
                            );
                            *slot = None;
                        }
                    }
                }
                for slot in side.volume.iter_mut() {
                    if let Some(st) = slot.as_ref() {
                        if st.last_seen_generation != gen {
                            print_debug!(
                                "stick repeat cleanup: controller={cid} repeat_slot=Volume"
                            );
                            *slot = None;
                        }
                    }
                }
                for slot in side.brightness.iter_mut() {
                    if let Some(st) = slot.as_ref() {
                        if st.last_seen_generation != gen {
                            print_debug!(
                                "stick repeat cleanup: controller={cid} repeat_slot=Brightness"
                            );
                            *slot = None;
                        }
                    }
                }
            }
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq_counter = self.seq_counter.wrapping_add(1);
        if self.seq_counter == 0 {
            self.seq_counter = 1;
        }
        self.seq_counter
    }

    fn push_due(&mut self, id: RepeatTaskId, seq: u64, due: Instant) {
        self.schedule.push(SchedEntry { due, id, seq });
    }

    fn entry_is_stale(&self, entry: &SchedEntry) -> bool {
        match self.slot_for(entry.id) {
            None => true,
            Some(st) => st.seq != entry.seq,
        }
    }

    fn slot_for(&self, id: RepeatTaskId) -> Option<&RepeatTaskState> {
        let ctrl = self.controllers.get(&id.controller)?;
        let side = &ctrl.sides[super::util::side_index(&id.side)];
        match id.kind {
            RepeatKind::Arrow(dir) => side.arrows[Self::dir_index(dir)].as_ref(),
            RepeatKind::Volume { axis, positive } => {
                side.volume[Self::step_slot_index(axis, positive)].as_ref()
            }
            RepeatKind::Brightness { axis, positive } => {
                side.brightness[Self::step_slot_index(axis, positive)].as_ref()
            }
        }
    }

    fn slot_for_mut(
        &mut self,
        id: &RepeatTaskId,
    ) -> Option<&mut Option<RepeatTaskState>> {
        let ctrl = self.controllers.get_mut(&id.controller)?;
        let side_idx = super::util::side_index(&id.side);
        let side = &mut ctrl.sides[side_idx];
        match id.kind {
            RepeatKind::Arrow(dir) => Some(&mut side.arrows[Self::dir_index(dir)]),
            RepeatKind::Volume { axis, positive } => {
                Some(&mut side.volume[Self::step_slot_index(axis, positive)])
            }
            RepeatKind::Brightness { axis, positive } => {
                Some(&mut side.brightness[Self::step_slot_index(axis, positive)])
            }
        }
    }
}

#[derive(Clone, Copy)]
struct SchedEntry {
    due: Instant,
    id: RepeatTaskId,
    seq: u64,
}

impl PartialEq for SchedEntry {
    fn eq(&self, other: &Self) -> bool {
        self.due.eq(&other.due) && self.seq == other.seq && self.id == other.id
    }
}
impl Eq for SchedEntry {}
impl PartialOrd for SchedEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SchedEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; reverse to make earliest due at the top
        other.due.cmp(&self.due)
    }
}
