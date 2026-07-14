use colored::Colorize;
use padjutsu_gamepad::ControllerId;
use padjutsu_workspace::{Axis as ProfileAxis, StickMode, StickSide};

use crate::app::Effect;
use crate::print_debug;

use super::compiled::CompiledStickRules;
use super::repeat::{
    Direction, MousePerfFrame, RepeatKind, RepeatTaskId, RepeatReg, SideRepeatState,
    StickProcessor,
};
use super::StepperMode;
use super::util::{
    axis_index, axes_for_side, invert_xy, magnitude2d, normalize_after_deadzone,
    normalize_with_outer_deadzone,
};

#[inline]
fn trigger_scroll_boost(
    axes: [f32; 6],
    params: &padjutsu_workspace::ScrollParams,
) -> (f32, f32) {
    let lt = axes[axis_index(padjutsu_gamepad::Axis::LeftTrigger)].max(0.0);
    let rt = axes[axis_index(padjutsu_gamepad::Axis::RightTrigger)].max(0.0);
    let trigger = lt.max(rt).clamp(0.0, 1.0);
    let boost = 1.0
        + params.runtime.trigger_boost_max
            * trigger.powf(params.runtime.trigger_boost_gamma);
    (trigger, boost)
}

impl StickProcessor {
    fn emit_mouse_move_chunked(
        sink: &mut impl FnMut(Effect),
        dx: i32,
        dy: i32,
    ) -> MousePerfFrame {
        const MAX_CHUNK_AXIS: i32 = 16;

        let mut perf = MousePerfFrame::default();
        let max_axis = dx.abs().max(dy.abs());
        if max_axis <= MAX_CHUNK_AXIS {
            sink(Effect::MouseMove { dx, dy });
            perf.move_events = 1;
            let chunk = ((dx * dx + dy * dy) as f64).sqrt();
            perf.distance_total = chunk;
            let chunk = chunk.round() as u64;
            perf.chunk_max = chunk;
            if chunk > 8 {
                perf.chunk_over_8 = 1;
            }
            if chunk > 16 {
                perf.chunk_over_16 = 1;
            }
            if chunk > 32 {
                perf.chunk_over_32 = 1;
            }
            return perf;
        }
        let steps = ((max_axis + MAX_CHUNK_AXIS - 1) / MAX_CHUNK_AXIS).max(1);
        let mut prev_x = 0;
        let mut prev_y = 0;

        for step in 1..=steps {
            let target_x = (dx * step) / steps;
            let target_y = (dy * step) / steps;
            let chunk_x = target_x - prev_x;
            let chunk_y = target_y - prev_y;
            prev_x = target_x;
            prev_y = target_y;
            if chunk_x == 0 && chunk_y == 0 {
                continue;
            }
            sink(Effect::MouseMove {
                dx: chunk_x,
                dy: chunk_y,
            });
            perf.move_events += 1;
            let chunk = ((chunk_x * chunk_x + chunk_y * chunk_y) as f64).sqrt();
            perf.distance_total += chunk;
            let chunk = chunk.round() as u64;
            perf.chunk_max = perf.chunk_max.max(chunk);
            if chunk > 8 {
                perf.chunk_over_8 += 1;
            }
            if chunk > 16 {
                perf.chunk_over_16 += 1;
            }
            if chunk > 32 {
                perf.chunk_over_32 += 1;
            }
        }

        perf
    }

    pub fn on_tick_with<F: FnMut(Effect)>(
        &mut self,
        bindings: Option<&CompiledStickRules>,
        axes_list: &[(ControllerId, [f32; 6])],
        precision: bool,
        mut sink: F,
    ) {
        if axes_list.is_empty() && !self.has_active_repeats() {
            return;
        }
        let Some(bindings) = bindings else {
            return;
        };

        let now = std::time::Instant::now();
        let started_at = now;
        let previous_tick_at = self.last_tick_at;
        let expected_tick_us = Self::expected_tick_us(bindings);
        let expected_tick_s = expected_tick_us as f32 / 1_000_000.0;
        let dt_s = self.tick_dt_s(now, expected_tick_s);
        let dt_us = previous_tick_at
            .map(|last_tick_at| {
                now.saturating_duration_since(last_tick_at).as_micros() as u64
            })
            .unwrap_or((dt_s * 1_000_000.0) as u64);
        self.generation = self.generation.wrapping_add(1);
        print_debug!(
            "stick processor tick: generation={} controllers={} has_repeats={} dt_s={dt_s:.4} left_mode={:?} right_mode={:?}",
            self.generation,
            axes_list.len(),
            self.has_active_repeats(),
            bindings.left(),
            bindings.right()
        );

        if matches!(bindings.left(), Some(StickMode::Arrows(_)))
            || matches!(bindings.right(), Some(StickMode::Arrows(_)))
        {
            self.tick_arrows(now, &mut sink, axes_list, bindings);
        }
        if matches!(bindings.left(), Some(StickMode::Volume(_)))
            || matches!(bindings.right(), Some(StickMode::Volume(_)))
        {
            self.tick_stepper(
                now,
                &mut sink,
                axes_list,
                bindings,
                StepperMode::Volume,
            );
        }
        if matches!(bindings.left(), Some(StickMode::Brightness(_)))
            || matches!(bindings.right(), Some(StickMode::Brightness(_)))
        {
            self.tick_stepper(
                now,
                &mut sink,
                axes_list,
                bindings,
                StepperMode::Brightness,
            );
        }
        let mut mouse_perf = MousePerfFrame::default();
        if matches!(bindings.left(), Some(StickMode::MouseMove(_)))
            || matches!(bindings.right(), Some(StickMode::MouseMove(_)))
        {
            mouse_perf =
                self.tick_mouse(dt_s, &mut sink, axes_list, bindings, precision);
        }
        let has_scroll = matches!(bindings.left(), Some(StickMode::Scroll(_)))
            || matches!(bindings.right(), Some(StickMode::Scroll(_)));
        if has_scroll {
            self.tick_scroll(dt_s, &mut sink, axes_list, bindings);
        }
        if self.generation % 500 == 1 {
            print_debug!(
                "stick modes: left={:?} right={:?} has_scroll={} axes=[{}]",
                bindings.left().map(std::mem::discriminant),
                bindings.right().map(std::mem::discriminant),
                has_scroll,
                axes_list
                    .iter()
                    .map(|(cid, a)| format!(
                        "c{cid}:LX={:.2},LY={:.2},RX={:.2},RY={:.2},LT={:.2},RT={:.2}",
                        a[0], a[1], a[2], a[3], a[4], a[5]
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }

        // Repeat draining is now event-driven, cleanup still needs to run per generation
        self.repeater_cleanup_inactive();
        let tick_elapsed_us = started_at.elapsed().as_micros() as u64;
        self.perf.samples += 1;
        self.perf.expected_tick_us = expected_tick_us;
        self.perf.tick_interval_us.record(dt_us);
        self.perf.tick_execution_us.record(tick_elapsed_us);
        self.perf.gap_over_1_5x +=
            u64::from(dt_us > expected_tick_us.saturating_mul(3) / 2);
        self.perf.gap_over_2x +=
            u64::from(dt_us > expected_tick_us.saturating_mul(2));
        self.perf.gap_over_4x +=
            u64::from(dt_us > expected_tick_us.saturating_mul(4));
        let elapsed_periods = dt_us
            .saturating_add(expected_tick_us / 2)
            .checked_div(expected_tick_us.max(1))
            .unwrap_or(1)
            .max(1);
        self.perf.missed_periods += elapsed_periods.saturating_sub(1);
        if mouse_perf.mode_active {
            self.perf.mouse_mode_ticks += 1;
            self.perf.mouse_move_events += mouse_perf.move_events;
            self.perf.mouse_distance_total += mouse_perf.distance_total;
            self.perf.mouse_chunk_max =
                self.perf.mouse_chunk_max.max(mouse_perf.chunk_max);
            self.perf.mouse_chunk_over_8 += mouse_perf.chunk_over_8;
            self.perf.mouse_chunk_over_16 += mouse_perf.chunk_over_16;
            self.perf.mouse_chunk_over_32 += mouse_perf.chunk_over_32;
            if mouse_perf.move_events == 0 {
                self.perf.mouse_zero_move_ticks += 1;
            }
        }
        if self.perf.last_report_at.is_none() {
            self.perf.last_report_at = Some(now);
        }
        let should_report = Self::metrics_enabled()
            && self.perf.last_report_at.is_some_and(|last| {
                now.saturating_duration_since(last)
                    >= Self::metrics_report_interval()
            });
        if should_report {
            eprintln!(
                "[stick-metrics] samples={} expected_tick_us={} tick_interval_us({}) tick_execution_us({}) gap_over_1_5x={} gap_over_2x={} gap_over_4x={} missed_periods={} mouse_mode_ticks={} mouse_move_events={} mouse_zero_move_ticks={} mouse_distance_total={:.1} mouse_chunk_max={} mouse_chunk_over_8={} mouse_chunk_over_16={} mouse_chunk_over_32={} scroll_events={}",
                self.perf.samples,
                self.perf.expected_tick_us,
                self.perf.tick_interval_us.summary(),
                self.perf.tick_execution_us.summary(),
                self.perf.gap_over_1_5x,
                self.perf.gap_over_2x,
                self.perf.gap_over_4x,
                self.perf.missed_periods,
                self.perf.mouse_mode_ticks,
                self.perf.mouse_move_events,
                self.perf.mouse_zero_move_ticks,
                self.perf.mouse_distance_total,
                self.perf.mouse_chunk_max,
                self.perf.mouse_chunk_over_8,
                self.perf.mouse_chunk_over_16,
                self.perf.mouse_chunk_over_32,
                self.perf.scroll_events
            );
            self.perf = super::repeat::TickPerfStats {
                last_report_at: Some(now),
                ..Default::default()
            };
        }
        print_debug!(
            "stick processor tick done: generation={} elapsed_us={}",
            self.generation,
            tick_elapsed_us
        );
    }

    fn metrics_enabled() -> bool {
        static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ENABLED.get_or_init(|| {
            std::env::var("PADJUTSU_METRICS")
                .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
                .unwrap_or(true)
        })
    }

    fn metrics_report_interval() -> std::time::Duration {
        static INTERVAL: std::sync::OnceLock<std::time::Duration> =
            std::sync::OnceLock::new();
        *INTERVAL.get_or_init(|| {
            let seconds = std::env::var("PADJUTSU_METRICS_INTERVAL_S")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(60)
                .clamp(5, 3_600);
            std::time::Duration::from_secs(seconds)
        })
    }

    fn expected_tick_us(bindings: &CompiledStickRules) -> u64 {
        [bindings.left(), bindings.right()]
            .into_iter()
            .filter_map(|mode| match mode {
                Some(StickMode::MouseMove(params)) => Some(params.runtime.tick_ms),
                Some(StickMode::Scroll(params)) => Some(params.runtime.tick_ms),
                _ => None,
            })
            .min()
            .unwrap_or(10)
            .saturating_mul(1_000)
    }

    fn tick_dt_s(&mut self, now: std::time::Instant, default_dt_s: f32) -> f32 {
        const MIN_DT_S: f32 = 0.001;
        const MAX_DT_S: f32 = 0.050;

        let dt_s = self
            .last_tick_at
            .map(|last_tick_at| {
                now.saturating_duration_since(last_tick_at).as_secs_f32()
            })
            .unwrap_or(default_dt_s)
            .clamp(MIN_DT_S, MAX_DT_S);
        self.last_tick_at = Some(now);
        dt_s
    }

    pub fn has_active_repeats(&self) -> bool {
        for (_cid, ctrl) in self.controllers.iter() {
            for side in ctrl.sides.iter() {
                if side.arrows.iter().any(|s| s.is_some())
                    || side.volume.iter().any(|s| s.is_some())
                    || side.brightness.iter().any(|s| s.is_some())
                {
                    return true;
                }
            }
        }
        false
    }

    pub fn has_active_repeats_for(&self, id: ControllerId) -> bool {
        let Some(ctrl) = self.controllers.get(&id) else {
            return false;
        };

        ctrl.sides.iter().any(|side| {
            side.arrows.iter().any(|s| s.is_some())
                || side.volume.iter().any(|s| s.is_some())
                || side.brightness.iter().any(|s| s.is_some())
        })
    }

    pub fn has_active_repeats_for_side(
        &self,
        id: ControllerId,
        side: StickSide,
    ) -> bool {
        let Some(ctrl) = self.controllers.get(&id) else {
            return false;
        };
        let side = &ctrl.sides[super::util::side_index(&side)];

        side.arrows.iter().any(|s| s.is_some())
            || side.volume.iter().any(|s| s.is_some())
            || side.brightness.iter().any(|s| s.is_some())
    }

    fn tick_arrows(
        &mut self,
        now: std::time::Instant,
        sink: &mut impl FnMut(Effect),
        axes_list: &[(ControllerId, [f32; 6])],
        bindings: &CompiledStickRules,
    ) {
        let mut regs = std::mem::take(&mut self.regs);
        regs.clear();
        for (id, axes) in axes_list.iter().cloned() {
            if let Some(StickMode::Arrows(params)) = bindings.left() {
                let (x0, y0) = axes_for_side(axes, &StickSide::Left);
                let (x, y) = invert_xy(x0, y0, params.invert_x, !params.invert_y);
                let mag2 = x * x + y * y;
                let dead2 = params.deadzone * params.deadzone;
                let new_dir = if mag2 < dead2 {
                    None
                } else {
                    Self::quantize_direction(x, y)
                };
                print_debug!(
                    "stick arrows: controller={id} side=Left raw=({x0:.3},{y0:.3}) adjusted=({x:.3},{y:.3}) mag2={mag2:.3} dead2={dead2:.3} dir={new_dir:?}"
                );
                if let Some(dir) = new_dir {
                    let task_id = RepeatTaskId {
                        controller: id,
                        side: StickSide::Left,
                        kind: RepeatKind::Arrow(dir),
                    };
                    let key = Self::get_direction_key(dir);
                    regs.push(RepeatReg {
                        id: task_id,
                        key,
                        fire_on_activate: true,
                        initial_delay_ms: params.repeat_delay_ms,
                        interval_ms: params.repeat_interval_ms,
                    });
                }
            }
            if let Some(StickMode::Arrows(params)) = bindings.right() {
                let (x0, y0) = axes_for_side(axes, &StickSide::Right);
                let (x, y) = invert_xy(x0, y0, params.invert_x, !params.invert_y);
                let mag2 = x * x + y * y;
                let dead2 = params.deadzone * params.deadzone;
                let new_dir = if mag2 < dead2 {
                    None
                } else {
                    Self::quantize_direction(x, y)
                };
                print_debug!(
                    "stick arrows: controller={id} side=Right raw=({x0:.3},{y0:.3}) adjusted=({x:.3},{y:.3}) mag2={mag2:.3} dead2={dead2:.3} dir={new_dir:?}"
                );
                if let Some(dir) = new_dir {
                    let task_id = RepeatTaskId {
                        controller: id,
                        side: StickSide::Right,
                        kind: RepeatKind::Arrow(dir),
                    };
                    let key = Self::get_direction_key(dir);
                    regs.push(RepeatReg {
                        id: task_id,
                        key,
                        fire_on_activate: true,
                        initial_delay_ms: params.repeat_delay_ms,
                        interval_ms: params.repeat_interval_ms,
                    });
                }
            }
        }
        for reg in regs.drain(..) {
            if let Some(a) = self.repeater_register(reg, now) {
                (sink)(a);
            }
        }
        self.regs = regs;
    }

    fn tick_stepper(
        &mut self,
        now: std::time::Instant,
        sink: &mut impl FnMut(Effect),
        axes_list: &[(ControllerId, [f32; 6])],
        bindings: &CompiledStickRules,
        mode: StepperMode,
    ) {
        let mut regs = std::mem::take(&mut self.regs);
        regs.clear();
        for (cid, axes) in axes_list.iter().cloned() {
            if let Some(step_params) = match (&mode, bindings.left()) {
                (StepperMode::Volume, Some(StickMode::Volume(p))) => Some(p),
                (StepperMode::Brightness, Some(StickMode::Brightness(p))) => Some(p),
                _ => None,
            } {
                let (vx, vy) = (
                    axes[axis_index(padjutsu_gamepad::Axis::LeftX)],
                    axes[axis_index(padjutsu_gamepad::Axis::LeftY)],
                );
                let v = match step_params.axis {
                    ProfileAxis::X => vx,
                    ProfileAxis::Y => vy,
                };
                let mag = v.abs();
                if mag >= step_params.deadzone {
                    let t = mag;
                    let interval_ms = (step_params.max_interval_ms as f32)
                        + (1.0 - t)
                            * ((step_params.min_interval_ms as f32)
                                - (step_params.max_interval_ms as f32));
                    let positive = v >= 0.0;
                    let key = mode.key_for(positive);
                    let kind = mode.kind_for(step_params.axis, positive);
                    let task_id = RepeatTaskId {
                        controller: cid,
                        side: StickSide::Left,
                        kind,
                    };
                    regs.push(RepeatReg {
                        id: task_id,
                        key,
                        fire_on_activate: true,
                        initial_delay_ms: 0,
                        interval_ms: interval_ms as u64,
                    });
                    print_debug!(
                        "stick stepper: controller={cid} side=Left mode={mode:?} axis={:?} value={v:.3} mag={mag:.3} interval_ms={} positive={positive}",
                        step_params.axis,
                        interval_ms as u64
                    );
                }
            }
            if let Some(step_params) = match (&mode, bindings.right()) {
                (StepperMode::Volume, Some(StickMode::Volume(p))) => Some(p),
                (StepperMode::Brightness, Some(StickMode::Brightness(p))) => Some(p),
                _ => None,
            } {
                let (vx, vy) = (
                    axes[axis_index(padjutsu_gamepad::Axis::RightX)],
                    axes[axis_index(padjutsu_gamepad::Axis::RightY)],
                );
                let v = match step_params.axis {
                    ProfileAxis::X => vx,
                    ProfileAxis::Y => vy,
                };
                let mag = v.abs();
                if mag >= step_params.deadzone {
                    let t = mag;
                    let interval_ms = (step_params.max_interval_ms as f32)
                        + (1.0 - t)
                            * ((step_params.min_interval_ms as f32)
                                - (step_params.max_interval_ms as f32));
                    let positive = v >= 0.0;
                    let key = mode.key_for(positive);
                    let kind = mode.kind_for(step_params.axis, positive);
                    let task_id = RepeatTaskId {
                        controller: cid,
                        side: StickSide::Right,
                        kind,
                    };
                    regs.push(RepeatReg {
                        id: task_id,
                        key,
                        fire_on_activate: true,
                        initial_delay_ms: 0,
                        interval_ms: interval_ms as u64,
                    });
                    print_debug!(
                        "stick stepper: controller={cid} side=Right mode={mode:?} axis={:?} value={v:.3} mag={mag:.3} interval_ms={} positive={positive}",
                        step_params.axis,
                        interval_ms as u64
                    );
                }
            }
        }
        for reg in regs.drain(..) {
            if let Some(a) = self.repeater_register(reg, now) {
                (sink)(a);
            }
        }
        self.regs = regs;
    }

    fn tick_mouse(
        &mut self,
        dt_s: f32,
        sink: &mut impl FnMut(Effect),
        axes_list: &[(ControllerId, [f32; 6])],
        bindings: &CompiledStickRules,
        precision: bool,
    ) -> MousePerfFrame {
        let mut perf = MousePerfFrame::default();
        for (_cid, axes) in axes_list.iter().cloned() {
            if let Some(StickMode::MouseMove(params)) = bindings.left() {
                perf.mode_active = true;
                let sidx = super::util::side_index(&StickSide::Left);
                let side =
                    &mut self.controllers.entry(_cid).or_default().sides[sidx];
                Self::tick_mouse_side(
                    dt_s,
                    params,
                    axes,
                    &StickSide::Left,
                    side,
                    precision,
                    sink,
                    &mut perf,
                );
            }
            if let Some(StickMode::MouseMove(params)) = bindings.right() {
                perf.mode_active = true;
                let sidx = super::util::side_index(&StickSide::Right);
                let side =
                    &mut self.controllers.entry(_cid).or_default().sides[sidx];
                Self::tick_mouse_side(
                    dt_s,
                    params,
                    axes,
                    &StickSide::Right,
                    side,
                    precision,
                    sink,
                    &mut perf,
                );
            }
        }
        perf
    }

    fn tick_mouse_side(
        dt_s: f32,
        params: &padjutsu_workspace::MouseParams,
        axes: [f32; 6],
        stick_side: &StickSide,
        side: &mut SideRepeatState,
        precision: bool,
        sink: &mut impl FnMut(Effect),
        perf: &mut MousePerfFrame,
    ) {
        let alpha =
            Self::mouse_smoothing_alpha(dt_s, params.runtime.smoothing_window_ms);
        let (x0, y0) = axes_for_side(axes, stick_side);
        let (raw_x, raw_y) = invert_xy(x0, y0, params.invert_x, params.invert_y);
        // Let the filter track raw input freely — never reset it.
        // This allows smooth zero-crossing during direction reversals
        // without getting trapped by the deadzone threshold.
        side.mouse_filtered.0 += alpha * (raw_x - side.mouse_filtered.0);
        side.mouse_filtered.1 += alpha * (raw_y - side.mouse_filtered.1);
        let (x, y) = side.mouse_filtered;
        let mag_raw = magnitude2d(x, y);
        if mag_raw >= params.deadzone {
            let base = normalize_with_outer_deadzone(
                mag_raw,
                params.deadzone,
                params.outer_deadzone,
            );
            let mag = Self::three_zone_curve(base, params.gamma);
            if mag > 0.0 {
                let dir_x = x / mag_raw;
                let dir_y = y / mag_raw;
                let speed_mul = if precision {
                    params.precision_multiplier
                } else {
                    1.0
                };
                let speed_px_s = params.max_speed_px_s * mag * speed_mul;
                let accum = &mut side.mouse_accum;
                accum.0 += speed_px_s * dir_x * dt_s;
                accum.1 += speed_px_s * dir_y * dt_s;
                let dx = accum.0.trunc() as i32;
                let dy = accum.1.trunc() as i32;
                if dx != 0 || dy != 0 {
                    let chunk_perf = Self::emit_mouse_move_chunked(sink, dx, dy);
                    perf.move_events += chunk_perf.move_events;
                    perf.distance_total += chunk_perf.distance_total;
                    perf.chunk_max = perf.chunk_max.max(chunk_perf.chunk_max);
                    perf.chunk_over_8 += chunk_perf.chunk_over_8;
                    perf.chunk_over_16 += chunk_perf.chunk_over_16;
                    perf.chunk_over_32 += chunk_perf.chunk_over_32;
                    accum.0 -= dx as f32;
                    accum.1 -= dy as f32;
                }
                // Clamp remainder to ±1px so direction changes respond
                // instantly when cursor is at a screen edge.
                accum.0 = accum.0.clamp(-1.0, 1.0);
                accum.1 = accum.1.clamp(-1.0, 1.0);
            }
        } else {
            // Only zero the movement accumulator, NOT the filter.
            side.mouse_accum = (0.0, 0.0);
        }
    }

    #[inline]
    fn mouse_smoothing_alpha(dt_s: f32, smoothing_window_ms: u64) -> f32 {
        let window_s = (smoothing_window_ms as f32 / 1000.0).max(0.001);
        (1.0 - (-dt_s / window_s).exp()).clamp(0.02, 0.55)
    }

    #[inline]
    fn fast_gamma(base: f32, gamma: f32) -> f32 {
        let g = gamma.max(0.1);
        if (g - 1.0).abs() < 1e-6 {
            base
        } else if (g - 0.5).abs() < 1e-6 {
            base.sqrt()
        } else if (g - 1.5).abs() < 1e-6 {
            base * base.sqrt()
        } else if (g - 2.0).abs() < 1e-6 {
            base * base
        } else if (g - 3.0).abs() < 1e-6 {
            base * base * base
        } else {
            base.powf(g)
        }
    }

    /// Three-zone response curve for mouse movement.
    ///
    /// - Zone 1 (0.0..0.4): reduced sensitivity (×0.4) for precision
    /// - Zone 2 (0.4..0.8): linear 1:1 for predictable movement
    /// - Zone 3 (0.8..1.0): accelerated (power curve with `gamma`) for fast traversal
    ///
    /// Output is continuous and spans [0.0, 1.0].
    #[inline]
    fn three_zone_curve(base: f32, gamma: f32) -> f32 {
        // Zone boundaries and output values at boundaries:
        // zone1: input [0, 0.4] → output [0, 0.16]       (slope 0.4)
        // zone2: input [0.4, 0.8] → output [0.16, 0.56]  (slope 1.0)
        // zone3: input [0.8, 1.0] → output [0.56, 1.0]   (accelerated)
        const Z1_END: f32 = 0.4;
        const Z2_END: f32 = 0.8;
        const SLOW_FACTOR: f32 = 0.4;
        // Output at zone boundaries
        const OUT_Z1: f32 = Z1_END * SLOW_FACTOR; // 0.16
        const OUT_Z2: f32 = OUT_Z1 + (Z2_END - Z1_END); // 0.56

        if base <= 0.0 {
            0.0
        } else if base <= Z1_END {
            // Zone 1: reduced sensitivity
            base * SLOW_FACTOR
        } else if base <= Z2_END {
            // Zone 2: linear
            OUT_Z1 + (base - Z1_END)
        } else {
            // Zone 3: accelerated — remap [0.8, 1.0] to [0, 1], apply gamma, remap to [0.56, 1.0]
            let t = ((base - Z2_END) / (1.0 - Z2_END)).min(1.0);
            let curved = Self::fast_gamma(t, gamma);
            OUT_Z2 + curved * (1.0 - OUT_Z2)
        }
    }

    fn tick_scroll(
        &mut self,
        dt_s: f32,
        sink: &mut impl FnMut(Effect),
        axes_list: &[(ControllerId, [f32; 6])],
        bindings: &CompiledStickRules,
    ) {
        for (cid, axes) in axes_list.iter().cloned() {
            if let Some(StickMode::Scroll(params)) = bindings.left() {
                self.tick_scroll_side(
                    cid,
                    axes,
                    StickSide::Left,
                    params,
                    dt_s,
                    sink,
                );
            }
            if let Some(StickMode::Scroll(params)) = bindings.right() {
                self.tick_scroll_side(
                    cid,
                    axes,
                    StickSide::Right,
                    params,
                    dt_s,
                    sink,
                );
            }
        }
    }

    fn tick_scroll_side(
        &mut self,
        cid: ControllerId,
        axes: [f32; 6],
        side: StickSide,
        params: &padjutsu_workspace::ScrollParams,
        dt_s: f32,
        sink: &mut impl FnMut(Effect),
    ) {
        let started_at = std::time::Instant::now();
        let alpha =
            Self::mouse_smoothing_alpha(dt_s, params.runtime.smoothing_window_ms);
        let side_label = match side {
            StickSide::Left => "Left",
            StickSide::Right => "Right",
        };
        let (x0, y0) = axes_for_side(axes, &side);
        let (raw_x, raw_y) = invert_xy(x0, y0, params.invert_x, params.invert_y);
        let sidx = super::util::side_index(&side);
        let side_state = &mut self.controllers.entry(cid).or_default().sides[sidx];
        side_state.scroll_filtered.0 +=
            alpha * (raw_x - side_state.scroll_filtered.0);
        side_state.scroll_filtered.1 +=
            alpha * (raw_y - side_state.scroll_filtered.1);

        // ── Deadzone ── identical to original (no axis_lock awareness) ──
        let mut x = side_state.scroll_filtered.0;
        let y = side_state.scroll_filtered.1;
        if !params.horizontal {
            x = 0.0;
        }
        let mag_raw = x.abs().max(y.abs());
        if mag_raw <= params.deadzone {
            if params.horizontal && params.axis_lock {
                side_state.scroll_idle_s += dt_s;
                if side_state.scroll_idle_s >= 0.2 {
                    side_state.scroll_axis_cum = (0.0, 0.0);
                }
            }
            // Don't reset scroll_filtered here — let the smoothing filter
            // track the raw values freely. Only reset the accumulator.
            side_state.scroll_accum = (0.0, 0.0);
            return;
        }
        side_state.scroll_idle_s = 0.0;

        let base = normalize_after_deadzone(mag_raw, params.deadzone);
        let mag = Self::fast_gamma(base, params.runtime.gamma);
        if mag <= 0.0 {
            return;
        }

        let scale = mag / mag_raw;
        let sx = x * scale;
        let sy = y * scale;
        let (trigger, trigger_boost) = trigger_scroll_boost(axes, params);
        print_debug!(
            "scroll pipeline: controller={cid} side={side_label} dt_s={dt_s:.4} raw=({x0:.3},{y0:.3}) filtered=({x:.3},{y:.3}) mag_raw={mag_raw:.3} base={base:.3} gamma={} mag={mag:.3} scale={scale:.3} speed_lines_s={} alpha={alpha:.3} trigger={trigger:.3} trigger_boost={trigger_boost:.3}",
            params.runtime.gamma,
            params.speed_lines_s
        );

        // ── Accumulate ──
        let accum = &mut side_state.scroll_accum;
        const SCROLL_SPEED_MULTIPLIER: f32 = 6.0;
        let speed = params.speed_lines_s * SCROLL_SPEED_MULTIPLIER * trigger_boost;
        accum.0 += speed * sx * dt_s;
        accum.1 += speed * sy * dt_s;

        // ── Axis lock: filter the OUTPUT, not the input ──
        // Track cumulative scroll per axis; lock to whichever has more.
        if params.horizontal && params.axis_lock {
            side_state.scroll_axis_cum.0 += accum.0.abs();
            side_state.scroll_axis_cum.1 += accum.1.abs();
            let (cx, cy) = side_state.scroll_axis_cum;
            if cy >= cx {
                accum.0 = 0.0; // suppress horizontal
            } else {
                accum.1 = 0.0; // suppress vertical
            }
        }

        let h = f64::from(accum.0);
        let v = f64::from(accum.1);
        if h.abs() >= 0.01 {
            print_debug!(
                "stick scroll: controller={cid} side={side_label} raw=({x0:.3},{y0:.3}) filtered=({x:.3},{y:.3}) mag={mag:.3} accum=({:.3},{:.3}) emit_h={h}",
                accum.0,
                accum.1
            );
            (sink)(Effect::Scroll { h, v: 0.0 });
            self.perf.scroll_events += 1;
            accum.0 = 0.0;
        }
        if v.abs() >= 0.01 {
            print_debug!(
                "stick scroll: controller={cid} side={side_label} raw=({x0:.3},{y0:.3}) filtered=({x:.3},{y:.3}) mag={mag:.3} accum=({:.3},{:.3}) emit_v={v}",
                accum.0,
                accum.1
            );
            (sink)(Effect::Scroll { h: 0.0, v });
            self.perf.scroll_events += 1;
            accum.1 = 0.0;
        }
        print_debug!(
            "scroll pipeline done: controller={cid} side={side_label} elapsed_us={} remaining_accum=({:.3},{:.3})",
            started_at.elapsed().as_micros(),
            accum.0,
            accum.1
        );
    }

    #[inline]
    pub fn quantize_direction(x: f32, y: f32) -> Option<Direction> {
        let ax = x.abs();
        let ay = y.abs();
        if ax == 0.0 && ay == 0.0 {
            return None;
        }
        if ax > ay {
            if x > 0.0 {
                Some(Direction::Right)
            } else {
                Some(Direction::Left)
            }
        } else if ay > ax {
            if y > 0.0 {
                Some(Direction::Up)
            } else {
                Some(Direction::Down)
            }
        } else if y > 0.0 {
            Some(Direction::Up)
        } else if y < 0.0 {
            Some(Direction::Down)
        } else {
            None
        }
    }

    #[inline]
    pub fn get_direction_key(dir: Direction) -> padjutsu_control::Key {
        match dir {
            Direction::Up => padjutsu_control::Key::UpArrow,
            Direction::Down => padjutsu_control::Key::DownArrow,
            Direction::Left => padjutsu_control::Key::LeftArrow,
            Direction::Right => padjutsu_control::Key::RightArrow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use padjutsu_workspace::{ScrollParams, ScrollRuntimeParams};

    fn scroll_params(horizontal: bool, axis_lock: bool) -> ScrollParams {
        ScrollParams {
            deadzone: 0.15,
            speed_lines_s: 120.0,
            horizontal,
            axis_lock,
            invert_x: false,
            invert_y: false,
            runtime: ScrollRuntimeParams {
                tick_ms: 4,
                smoothing_window_ms: 25,
                gamma: 1.5,
                trigger_boost_max: 0.0,
                trigger_boost_gamma: 1.5,
            },
        }
    }

    /// Create axes array with right stick values (indices 2=RightX, 3=RightY).
    fn right_stick_axes(x: f32, y: f32) -> [f32; 6] {
        [0.0, 0.0, x, y, 0.0, 0.0]
    }

    fn collect_scroll_effects(effects: &[Effect]) -> Vec<(f64, f64)> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Scroll { h, v } => Some((*h, *v)),
                _ => None,
            })
            .collect()
    }

    /// Tick scroll multiple times with configurable dt.
    fn tick_scroll_n_dt(
        proc: &mut StickProcessor,
        params: &ScrollParams,
        axes: [f32; 6],
        n: usize,
        dt_s: f32,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        for _ in 0..n {
            proc.tick_scroll_side(
                1,
                axes,
                StickSide::Right,
                params,
                dt_s,
                &mut |e| effects.push(e),
            );
        }
        effects
    }

    /// Tick scroll multiple times with default dt (10ms).
    fn tick_scroll_n(
        proc: &mut StickProcessor,
        params: &ScrollParams,
        axes: [f32; 6],
        n: usize,
    ) -> Vec<Effect> {
        tick_scroll_n_dt(proc, params, axes, n, 0.010)
    }

    // --- axis_lock tests ---

    #[test]
    fn axis_lock_locks_to_vertical_when_y_dominant() {
        let mut proc = StickProcessor::new();
        let params = scroll_params(true, true);

        // Push stick mostly down (y=0.8) with slight horizontal drift (x=0.1)
        let effects =
            tick_scroll_n(&mut proc, &params, right_stick_axes(0.1, 0.8), 20);
        let scrolls = collect_scroll_effects(&effects);

        // Should have vertical scroll but NO horizontal scroll
        assert!(!scrolls.is_empty(), "should produce scroll effects");
        for (h, _v) in &scrolls {
            assert_eq!(*h, 0.0, "horizontal scroll should be locked out");
        }
        assert!(
            scrolls.iter().any(|(_, v)| *v != 0.0),
            "should have vertical scroll"
        );
    }

    #[test]
    fn axis_lock_locks_to_horizontal_when_x_dominant() {
        let mut proc = StickProcessor::new();
        let params = scroll_params(true, true);

        // Push stick mostly right (x=0.8) with slight vertical drift (y=0.1)
        let effects =
            tick_scroll_n(&mut proc, &params, right_stick_axes(0.8, 0.1), 20);
        let scrolls = collect_scroll_effects(&effects);

        assert!(!scrolls.is_empty(), "should produce scroll effects");
        for (_h, v) in &scrolls {
            assert_eq!(*v, 0.0, "vertical scroll should be locked out");
        }
        assert!(
            scrolls.iter().any(|(h, _)| *h != 0.0),
            "should have horizontal scroll"
        );
    }

    #[test]
    fn axis_lock_resets_when_stick_returns_to_deadzone() {
        let mut proc = StickProcessor::new();
        let params = scroll_params(true, true);

        // First: scroll vertically to lock axis
        tick_scroll_n(&mut proc, &params, right_stick_axes(0.1, 0.8), 10);

        // Return to deadzone for 200ms+ (each tick is 10ms, so 25 ticks = 250ms)
        tick_scroll_n(&mut proc, &params, right_stick_axes(0.0, 0.0), 25);

        // Now scroll horizontally — should lock to horizontal
        let effects =
            tick_scroll_n(&mut proc, &params, right_stick_axes(0.8, 0.1), 20);
        let scrolls = collect_scroll_effects(&effects);

        assert!(!scrolls.is_empty(), "should produce scroll effects");
        for (_h, v) in &scrolls {
            assert_eq!(*v, 0.0, "vertical scroll should be locked out after reset");
        }
    }

    #[test]
    fn no_axis_lock_allows_both_axes() {
        let mut proc = StickProcessor::new();
        let params = scroll_params(true, false); // horizontal=true, axis_lock=false

        // Push stick diagonally
        let effects =
            tick_scroll_n(&mut proc, &params, right_stick_axes(0.5, 0.5), 20);
        let scrolls = collect_scroll_effects(&effects);

        assert!(!scrolls.is_empty(), "should produce scroll effects");
        let has_h = scrolls.iter().any(|(h, _)| *h != 0.0);
        let has_v = scrolls.iter().any(|(_, v)| *v != 0.0);
        assert!(has_h, "should have horizontal scroll without axis_lock");
        assert!(has_v, "should have vertical scroll without axis_lock");
    }

    #[test]
    fn scroll_works_with_small_dt() {
        // Regression: with 2ms ticks (dt=0.002), the smoothing filter alpha is very small.
        // If scroll_filtered is reset to zero in the deadzone branch, the filter can never
        // ramp past the deadzone threshold in a single tick, causing scroll to never start.
        let mut proc = StickProcessor::new();
        let params = scroll_params(false, false);

        // Simulate 2ms ticks — 100 ticks = 200ms, plenty of time to ramp up
        let effects = tick_scroll_n_dt(
            &mut proc,
            &params,
            right_stick_axes(0.0, 0.8),
            100,
            0.002,
        );
        let scrolls = collect_scroll_effects(&effects);

        assert!(
            !scrolls.is_empty(),
            "scroll must work with 2ms tick interval"
        );
        assert!(
            scrolls.iter().any(|(_, v)| *v != 0.0),
            "should have vertical scroll with small dt"
        );
    }

    #[test]
    fn scroll_survives_direction_change() {
        // When changing direction (up→down), the stick briefly crosses through
        // the deadzone. Scroll should resume without interruption.
        let mut proc = StickProcessor::new();
        let params = scroll_params(false, false);

        // Scroll down
        let effects1 =
            tick_scroll_n(&mut proc, &params, right_stick_axes(0.0, 0.8), 20);
        let scrolls1 = collect_scroll_effects(&effects1);
        assert!(!scrolls1.is_empty(), "initial scroll should work");

        // Brief pass through center (2 ticks = 20ms, simulates direction change)
        tick_scroll_n(&mut proc, &params, right_stick_axes(0.0, 0.0), 2);

        // Scroll up — should start quickly
        let effects2 =
            tick_scroll_n(&mut proc, &params, right_stick_axes(0.0, -0.8), 20);
        let scrolls2 = collect_scroll_effects(&effects2);
        assert!(
            !scrolls2.is_empty(),
            "scroll must resume after direction change"
        );
    }

    #[test]
    fn horizontal_false_zeroes_x_axis() {
        let mut proc = StickProcessor::new();
        let params = scroll_params(false, false); // horizontal=false

        // Push stick diagonally
        let effects =
            tick_scroll_n(&mut proc, &params, right_stick_axes(0.5, 0.8), 20);
        let scrolls = collect_scroll_effects(&effects);

        assert!(!scrolls.is_empty(), "should produce scroll effects");
        for (h, _v) in &scrolls {
            assert_eq!(
                *h, 0.0,
                "horizontal scroll should be zero when horizontal=false"
            );
        }
    }

    #[test]
    fn reset_tick_clock_uses_nominal_period_after_idle() {
        let mut processor = StickProcessor::new();
        let now = std::time::Instant::now();
        processor.last_tick_at = Some(now - std::time::Duration::from_millis(100));

        processor.reset_tick_clock();
        let dt_s = processor.tick_dt_s(now, 0.008);

        assert!((dt_s - 0.008).abs() < f32::EPSILON);
    }
}
