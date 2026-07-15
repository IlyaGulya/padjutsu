use std::cell::RefCell;
use std::time::Instant;
use ahash::AHashMap;

use colored::Colorize;

use padjutsu_control::KeyCombo;
use padjutsu_bit_mask::Bitmask;
use padjutsu_gamepad::{
    Axis as CtrlAxis, AxisSnapshot, Button, ControllerId, ControllerInfo,
};
use padjutsu_workspace::{
    ButtonAction, ControllerSettings, Profile, StickMode, StickSide,
};

use crate::{app::ButtonPhase, print_debug, print_info};
use super::binding::{BindingContext, BindingSource};
use super::effect::Effect;
use super::stick::{StickProcessor, CompiledStickRules};
use super::stick::util::axis_index as stick_axis_index;

#[derive(Debug)]
struct ControllerState {
    mapping: ControllerSettings,
    pressed: Bitmask<Button>,
    rumble: bool,
    axes: [f32; 6],
}

const DEFAULT_REPEAT_DELAY_MS: u64 = 400;
const DEFAULT_REPEAT_INTERVAL_MS: u64 = 50;

struct ButtonRepeatTask {
    key: KeyCombo,
    interval_ms: u64,
    next_fire: Instant,
    delay_done: bool,
}

#[derive(Debug, Default)]
struct ButtonRepeatPoll {
    effects: Vec<Effect>,
    fired: usize,
}

#[derive(Debug, Clone)]
struct ButtonTransition {
    target_button: Button,
    effects: Vec<Effect>,
    repeat: ButtonRepeatDirective,
}

#[derive(Debug, Clone)]
enum ButtonRepeatDirective {
    None,
    Start {
        key: KeyCombo,
        delay_ms: u64,
        interval_ms: u64,
    },
    Stop,
}

pub struct Padjutsu {
    pub workspace: Option<Profile>,
    active_app: Box<str>,
    binding: BindingContext,
    controllers: AHashMap<ControllerId, ControllerState>,
    sticks: RefCell<StickProcessor>,
    axes_scratch: Vec<(ControllerId, [f32; 6])>,
    button_repeats: AHashMap<(ControllerId, Button), ButtonRepeatTask>,
}

impl Default for Padjutsu {
    fn default() -> Self {
        Self::new()
    }
}

impl Padjutsu {
    pub fn new() -> Self {
        Self {
            workspace: None,
            active_app: "".into(),
            binding: BindingContext::default(),
            controllers: AHashMap::new(),
            sticks: RefCell::new(StickProcessor::new()),
            axes_scratch: Vec::new(),
            button_repeats: AHashMap::new(),
        }
    }

    pub fn is_known(&self, id: ControllerId) -> bool {
        self.controllers.contains_key(&id)
    }

    pub fn remove_workspace(&mut self) {
        self.workspace = None;
        self.binding = BindingContext::empty(self.active_app.as_ref());
        self.button_repeats.clear();
    }

    pub fn set_workspace(&mut self, workspace: Profile) {
        self.workspace = Some(workspace);
        self.button_repeats.clear();
        self.rebuild_binding_context();
    }

    pub fn add_controller(&mut self, info: ControllerInfo) {
        print_info!(
            "add controller - {0} id={1} vid=0x{2:x} pid=0x{3:x}",
            info.name,
            info.id,
            info.vendor_id,
            info.product_id
        );

        let settings = self
            .workspace
            .as_ref()
            .and_then(|ws| {
                ws.controllers
                    .get(&(info.vendor_id, info.product_id))
                    .cloned()
            })
            .unwrap_or_default();
        let state = ControllerState {
            mapping: settings,
            pressed: Bitmask::empty(),
            rumble: info.supports_rumble,
            axes: [0.0; 6],
        };
        if self.is_known(info.id) {
            print_debug!("controller already known - id={0}", info.id);
        }
        self.controllers.insert(info.id, state);
    }

    pub fn remove_controller(&mut self, id: ControllerId) {
        print_info!("remove device - {id:x}");
        self.controllers.remove(&id);
    }

    pub fn controller_has_pressed_buttons(&self, id: ControllerId) -> bool {
        self.controllers
            .get(&id)
            .is_some_and(|state| !state.pressed.is_empty())
    }

    pub fn controller_has_repeats(&self, id: ControllerId) -> bool {
        self.button_repeats.keys().any(|(cid, _)| *cid == id)
            || self.sticks.borrow().has_active_repeats_for(id)
    }

    pub fn controller_stick_has_repeats(
        &self,
        id: ControllerId,
        side: StickSide,
    ) -> bool {
        self.sticks.borrow().has_active_repeats_for_side(id, side)
    }

    pub fn controller_stick_has_axis_activity(
        &self,
        id: ControllerId,
        side: StickSide,
        threshold: f32,
    ) -> bool {
        let Some(state) = self.controllers.get(&id) else {
            return false;
        };
        let (x, y) = match side {
            StickSide::Left => (state.axes[0], state.axes[1]),
            StickSide::Right => (state.axes[2], state.axes[3]),
        };
        x.abs() >= threshold || y.abs() >= threshold
    }

    pub fn controller_stick_mode(
        &self,
        _id: ControllerId,
        side: StickSide,
    ) -> Option<&StickMode> {
        self.get_compiled_stick_rules()
            .and_then(|rules| match side {
                StickSide::Left => rules.left(),
                StickSide::Right => rules.right(),
            })
    }

    pub fn set_active_app(&mut self, app: &str) {
        if self.active_app.as_ref() == app {
            return;
        }
        if self.active_app.as_ref() == "" {
            print_debug!("got active app - {app}");
        } else {
            print_debug!("app change - {app}");
        }

        self.active_app = app.into();
        self.sticks.borrow_mut().on_app_change();
        self.button_repeats.clear();
        self.rebuild_binding_context();
    }

    pub fn get_compiled_stick_rules(&self) -> Option<&CompiledStickRules> {
        self.binding.compiled_stick_rules()
    }

    pub fn current_shell(&self) -> Option<Box<str>> {
        self.binding.shell().map(Into::into)
    }

    pub fn on_axis_motion(&mut self, id: ControllerId, axis: CtrlAxis, value: f32) {
        let idx = stick_axis_index(axis);
        if let Some(st) = self.controllers.get_mut(&id) {
            let prev = st.axes[idx];
            st.axes[idx] = value;
            let threshold = 0.05;
            let became_active = prev.abs() < threshold && value.abs() >= threshold;
            let became_idle = prev.abs() >= threshold && value.abs() < threshold;
            let changed_significantly = (prev - value).abs() >= 0.1;
            if became_active || became_idle || changed_significantly {
                print_debug!(
                    "axis motion: controller={id} axis={axis:?} value={value:.3} prev={prev:.3} active={}",
                    value.abs() >= threshold
                );
            }
        } else {
            print_debug!(
                "axis motion ignored for unknown controller={id} axis={axis:?}"
            );
        }
    }

    /// Replace queued axis history with the latest state read directly by the
    /// gamepad runtime. Returns true when stale local state was corrected.
    pub fn sync_axis_snapshot(
        &mut self,
        id: ControllerId,
        axes: AxisSnapshot,
    ) -> bool {
        let Some(state) = self.controllers.get_mut(&id) else {
            return false;
        };
        if state.axes == axes {
            return false;
        }
        state.axes = axes;
        true
    }

    pub fn has_active_mouse_axis_input(&self) -> bool {
        let Some(bindings) = self.get_compiled_stick_rules() else {
            return false;
        };
        self.controllers.values().any(|state| {
            let left_active = bindings.left().is_some_and(|mode| match mode {
                StickMode::MouseMove(params) => {
                    state.axes[0].hypot(state.axes[1]) >= params.deadzone
                }
                _ => false,
            });
            let right_active = bindings.right().is_some_and(|mode| match mode {
                StickMode::MouseMove(params) => {
                    state.axes[2].hypot(state.axes[3]) >= params.deadzone
                }
                _ => false,
            });
            left_active || right_active
        })
    }

    pub fn on_controller_disconnected(&mut self, id: ControllerId) {
        self.sticks.borrow_mut().release_all_for(id);
        // Clear any active button repeat tasks for this controller
        self.button_repeats.retain(|(cid, _), _| *cid != id);
    }

    fn is_precision_active(&self, bindings: Option<&CompiledStickRules>) -> bool {
        let Some(bindings) = bindings else {
            return false;
        };
        // Collect precision buttons from all mouse_move stick modes.
        let mut buttons: Vec<Button> = Vec::new();
        for mode in [bindings.left(), bindings.right()] {
            if let Some(StickMode::MouseMove(params)) = mode {
                if let Some(btn) = params.precision_button {
                    buttons.push(btn);
                }
            }
        }
        if buttons.is_empty() {
            return false;
        }
        self.controllers
            .values()
            .any(|st| buttons.iter().any(|btn| st.pressed.contains(*btn)))
    }

    pub fn on_tick_with<F: FnMut(Effect)>(&mut self, sink: F) {
        let started_at = Instant::now();
        let bindings_owned = self.get_compiled_stick_rules().cloned();
        let precision = self.is_precision_active(bindings_owned.as_ref());
        self.axes_scratch.clear();
        self.axes_scratch.reserve(self.controllers.len());
        for (id, st) in self.controllers.iter() {
            self.axes_scratch.push((*id, st.axes));
        }
        print_debug!(
            "stick tick: controllers={} axes_snapshots={} has_bindings={} active_repeats={} button_repeats={} precision={}",
            self.controllers.len(),
            self.axes_scratch.len(),
            bindings_owned.is_some(),
            self.sticks.borrow().has_active_repeats(),
            self.button_repeats.len(),
            precision
        );
        self.sticks.borrow_mut().on_tick_with(
            bindings_owned.as_ref(),
            &self.axes_scratch,
            precision,
            sink,
        );
        print_debug!(
            "stick tick done: elapsed_us={}",
            started_at.elapsed().as_micros()
        );
    }

    pub fn on_tick_effects(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.on_tick_with(|effect| effects.push(effect));
        effects
    }

    pub fn reset_tick_clock(&self) {
        self.sticks.borrow_mut().reset_tick_clock();
    }

    /// Return next due time for any repeat task, if any.
    pub fn next_repeat_due(&self) -> Option<std::time::Instant> {
        // Borrow mutably internally to read/update heap staleness cheaply.
        // Safety: RefCell ensures single mutable borrow.
        self.sticks.borrow_mut().next_repeat_due()
    }

    /// Process repeat tasks due up to `now`.
    pub fn process_due_repeats<F: FnMut(Effect)>(
        &self,
        now: std::time::Instant,
        mut sink: F,
    ) {
        let started_at = Instant::now();
        print_debug!("processing stick repeats at {now:?}");
        self.sticks.borrow_mut().process_due_repeats(now, &mut sink);
        print_debug!(
            "processing stick repeats done: elapsed_us={}",
            started_at.elapsed().as_micros()
        );
    }

    pub fn due_repeat_effects(&self, now: Instant) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.process_due_repeats(now, |effect| effects.push(effect));
        effects
    }

    /// Return next due time for any button repeat task, if any.
    pub fn next_button_repeat_due(&self) -> Option<Instant> {
        self.button_repeats.values().map(|t| t.next_fire).min()
    }

    /// Process button repeat tasks due up to `now`.
    pub fn process_button_repeats<F: FnMut(Effect)>(
        &mut self,
        now: Instant,
        sink: &mut F,
    ) {
        let started_at = Instant::now();
        let poll = self.poll_button_repeats(now);
        for effect in poll.effects {
            sink(effect);
        }
        if poll.fired > 0 {
            print_debug!(
                "processed button repeats: fired={} elapsed_us={}",
                poll.fired,
                started_at.elapsed().as_micros()
            );
        }
    }

    pub fn button_repeat_effects(&mut self, now: Instant) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.process_button_repeats(now, &mut |effect| effects.push(effect));
        effects
    }

    /// Whether any button repeat tasks are active.
    pub fn has_active_button_repeats(&self) -> bool {
        !self.button_repeats.is_empty()
    }

    /// Whether any periodic processing is needed right now.
    /// True when there are tick-requiring stick modes and some axis deviates from neutral,
    /// or when repeat tasks are active (to drain their timers).
    pub fn needs_tick(&self) -> bool {
        let has_tick_modes = self.has_tick_modes();
        let has_axis_activity = self.has_axis_activity(0.05);
        let has_stick_repeats = self.sticks.borrow().has_active_repeats();
        let has_button_repeats = self.has_active_button_repeats();
        let needs_tick = (has_tick_modes && has_axis_activity)
            || has_stick_repeats
            || has_button_repeats;
        if needs_tick {
            print_debug!(
                "needs_tick=true tick_modes={has_tick_modes} axis_activity={has_axis_activity} stick_repeats={has_stick_repeats} button_repeats={has_button_repeats}"
            );
        }
        needs_tick
    }

    /// Hint whether a faster tick would improve responsiveness.
    /// True when there is recent/ongoing axis activity or repeat tasks are active.
    pub fn wants_fast_tick(&self) -> bool {
        let axis_activity = self.has_axis_activity(0.05);
        let stick_repeats = self.sticks.borrow().has_active_repeats();
        let button_repeats = self.has_active_button_repeats();
        let fast = axis_activity || stick_repeats || button_repeats;
        if fast {
            print_debug!(
                "wants_fast_tick=true axis_activity={axis_activity} stick_repeats={stick_repeats} button_repeats={button_repeats}"
            );
        }
        fast
    }

    pub fn wants_continuous_tick_mode(&self) -> bool {
        self.has_continuous_stick_mode() && self.has_axis_activity(0.05)
    }

    pub fn continuous_tick_ms(&self) -> Option<u64> {
        let bindings = self.get_compiled_stick_rules()?;
        let mut tick_ms: Option<u64> = None;

        for side in [bindings.left(), bindings.right()] {
            let candidate_tick_ms = match side {
                Some(StickMode::MouseMove(params)) => Some(params.runtime.tick_ms),
                Some(StickMode::Scroll(params)) => Some(params.runtime.tick_ms),
                _ => None,
            };
            let Some(candidate_tick_ms) = candidate_tick_ms else {
                continue;
            };
            tick_ms = Some(match tick_ms {
                Some(current_tick_ms) => current_tick_ms.min(candidate_tick_ms),
                None => candidate_tick_ms,
            });
        }

        tick_ms
    }

    /// Whether the current profile has any stick modes that require periodic ticks.
    fn has_tick_modes(&self) -> bool {
        let Some(bindings) = self.get_compiled_stick_rules() else {
            return false;
        };
        matches!(
            bindings.left(),
            Some(
                StickMode::Arrows(_)
                    | StickMode::Volume(_)
                    | StickMode::Brightness(_)
                    | StickMode::MouseMove(_)
                    | StickMode::Scroll(_)
            )
        ) || matches!(
            bindings.right(),
            Some(
                StickMode::Arrows(_)
                    | StickMode::Volume(_)
                    | StickMode::Brightness(_)
                    | StickMode::MouseMove(_)
                    | StickMode::Scroll(_)
            )
        )
    }

    fn has_continuous_stick_mode(&self) -> bool {
        let Some(bindings) = self.binding.stick_rules() else {
            return false;
        };

        matches!(
            bindings.get(&padjutsu_workspace::StickSide::Left),
            Some(StickMode::MouseMove(_)) | Some(StickMode::Scroll(_))
        ) || matches!(
            bindings.get(&padjutsu_workspace::StickSide::Right),
            Some(StickMode::MouseMove(_)) | Some(StickMode::Scroll(_))
        )
    }

    /// Detect if any controller axis deviates beyond a small threshold.
    fn has_axis_activity(&self, threshold: f32) -> bool {
        if self.controllers.is_empty() {
            return false;
        }
        for (_id, st) in self.controllers.iter() {
            for v in st.axes.iter() {
                if v.abs() >= threshold {
                    return true;
                }
            }
        }
        false
    }

    pub fn on_button_with<F: FnMut(Effect)>(
        &mut self,
        id: ControllerId,
        button: Button,
        phase: ButtonPhase,
        mut sink: F,
    ) {
        print_debug!("handle button - {id} {button:?} {phase:?}");
        if self.binding.is_blacklisted() {
            print_debug!(
                "ignoring button for blacklisted app {}",
                self.binding.active_app()
            );
            return;
        };
        let Some(button_rules) = self.binding.button_rules() else {
            print_debug!(
                "no button rules in binding context for app={} source={:?}",
                self.binding.active_app(),
                self.binding.source()
            );
            return;
        };
        let Some(state) = self.controllers.get_mut(&id) else {
            print_debug!("ignoring button for unknown controller {id}");
            return;
        };
        let button = *state.mapping.mapping.get(&button).unwrap_or(&button);
        let rumble = state.rumble;

        // snapshot before change
        let prev_pressed = state.pressed;

        if phase == ButtonPhase::Pressed {
            state.pressed.insert(button);
        } else {
            state.pressed.remove(button);
        }

        // snapshot after change — drop mutable borrow of controllers after this
        let now_pressed = state.pressed;

        let transitions = Self::resolve_button_transitions(
            button_rules,
            prev_pressed,
            now_pressed,
            phase,
            id,
            button,
            rumble,
        );

        if transitions.is_empty() {
            print_debug!(
                "no matching rule for pressed={now_pressed:?} (button_rules={})",
                button_rules.len()
            );
            return;
        }

        for transition in transitions {
            self.apply_button_transition(id, transition, &mut sink);
        }
    }

    pub fn on_button_effects(
        &mut self,
        id: ControllerId,
        button: Button,
        phase: ButtonPhase,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.on_button_with(id, button, phase, |effect| effects.push(effect));
        effects
    }

    fn resolve_button_transitions(
        button_rules: &padjutsu_workspace::ButtonRules,
        prev_pressed: Bitmask<Button>,
        now_pressed: Bitmask<Button>,
        phase: ButtonPhase,
        id: ControllerId,
        button: Button,
        rumble: bool,
    ) -> Vec<ButtonTransition> {
        let mut transitions = Vec::new();

        if phase == ButtonPhase::Released {
            for (target, rule) in button_rules.iter() {
                let was = prev_pressed.is_superset(target);
                let is_now = now_pressed.is_superset(target);
                if !was || is_now {
                    continue;
                }

                let mut effects = Vec::new();
                let repeat = match rule.action.clone() {
                    ButtonAction::Keystroke(_) => ButtonRepeatDirective::Stop,
                    ButtonAction::HoldKeystroke(k) => {
                        effects.push(Effect::KeyRelease((*k).clone()));
                        ButtonRepeatDirective::None
                    }
                    ButtonAction::HoldClick(btn) => {
                        effects.push(Effect::MouseRelease { button: btn });
                        ButtonRepeatDirective::None
                    }
                    ButtonAction::RawModifier(key) => {
                        effects.push(Effect::RawModifierRelease(key));
                        ButtonRepeatDirective::None
                    }
                    _ => ButtonRepeatDirective::None,
                };
                transitions.push(ButtonTransition {
                    target_button: button,
                    effects,
                    repeat,
                });
            }

            return transitions;
        }

        // First pass: find max_bits among rules that should fire
        let mut max_bits: u32 = 0;
        for (target, _rule) in button_rules.iter() {
            let was = prev_pressed.is_superset(target);
            let is_now = now_pressed.is_superset(target);
            let fire = was != is_now;
            if fire {
                let bits: u32 = target.count();
                if bits > max_bits {
                    max_bits = bits;
                }
            }
        }
        if max_bits == 0 {
            return transitions;
        }
        print_debug!("firing rule with max_bits={max_bits}");

        // Second pass: execute only rules with that cardinality
        for (target, rule) in button_rules.iter() {
            let was = prev_pressed.is_superset(target);
            let is_now = now_pressed.is_superset(target);
            let fire = was != is_now;
            if !fire || target.count() != max_bits {
                continue;
            }
            match phase {
                ButtonPhase::Pressed => {
                    let mut effects = Vec::new();
                    if let Some(ms) = rule.vibrate {
                        if rumble {
                            effects.push(Effect::Rumble { id, ms: ms as u32 });
                        }
                    }
                    let repeat = match rule.action.clone() {
                        ButtonAction::Keystroke(k) => {
                            effects.push(Effect::KeyTap((*k).clone()));
                            ButtonRepeatDirective::Start {
                                key: (*k).clone(),
                                delay_ms: rule
                                    .repeat_delay_ms
                                    .unwrap_or(DEFAULT_REPEAT_DELAY_MS),
                                interval_ms: rule
                                    .repeat_interval_ms
                                    .unwrap_or(DEFAULT_REPEAT_INTERVAL_MS),
                            }
                        }
                        ButtonAction::HoldKeystroke(k) => {
                            effects.push(Effect::KeyPress((*k).clone()));
                            ButtonRepeatDirective::None
                        }
                        ButtonAction::TapKeystroke(k) => {
                            effects.push(Effect::KeyTap((*k).clone()));
                            ButtonRepeatDirective::None
                        }
                        ButtonAction::Macros(m) => {
                            effects.push(Effect::Macros(m));
                            ButtonRepeatDirective::None
                        }
                        ButtonAction::Shell(s) => {
                            print_debug!("shell command: {}", s);
                            effects.push(Effect::Shell(s));
                            ButtonRepeatDirective::None
                        }
                        ButtonAction::MouseClick { button, click_type } => {
                            effects.push(Effect::MouseClick { button, click_type });
                            ButtonRepeatDirective::None
                        }
                        ButtonAction::HoldClick(btn) => {
                            effects.push(Effect::MousePress { button: btn });
                            ButtonRepeatDirective::None
                        }
                        ButtonAction::RawModifier(key) => {
                            effects.push(Effect::RawModifierPress(key));
                            ButtonRepeatDirective::None
                        }
                    };
                    transitions.push(ButtonTransition {
                        target_button: button,
                        effects,
                        repeat,
                    });
                }
                ButtonPhase::Released => unreachable!(),
            }
        }

        transitions
    }

    fn apply_button_transition<F: FnMut(Effect)>(
        &mut self,
        id: ControllerId,
        transition: ButtonTransition,
        sink: &mut F,
    ) {
        for effect in transition.effects {
            sink(effect);
        }

        match transition.repeat {
            ButtonRepeatDirective::None => {}
            ButtonRepeatDirective::Start {
                key,
                delay_ms,
                interval_ms,
            } => {
                self.button_repeats.insert(
                    (id, transition.target_button),
                    ButtonRepeatTask {
                        key,
                        interval_ms,
                        next_fire: Instant::now()
                            + std::time::Duration::from_millis(delay_ms),
                        delay_done: false,
                    },
                );
            }
            ButtonRepeatDirective::Stop => {
                self.button_repeats.remove(&(id, transition.target_button));
            }
        }
    }

    fn poll_button_repeats(&mut self, now: Instant) -> ButtonRepeatPoll {
        let mut poll = ButtonRepeatPoll::default();

        for task in self.button_repeats.values_mut() {
            if now < task.next_fire {
                continue;
            }

            poll.effects.push(Effect::KeyTap(task.key.clone()));
            poll.fired += 1;
            if !task.delay_done {
                task.delay_done = true;
            }
            task.next_fire =
                now + std::time::Duration::from_millis(task.interval_ms);
        }

        poll
    }

    fn rebuild_binding_context(&mut self) {
        self.binding = BindingContext::rebuild(
            self.workspace.as_ref(),
            self.active_app.as_ref(),
        );
        print_debug!(
            "binding context rebuilt: app={} source={:?} has_buttons={} has_sticks={} has_compiled={} shell={}",
            self.binding.active_app(),
            self.binding.source(),
            self.binding.button_rules().is_some(),
            self.binding.stick_rules().is_some(),
            self.binding.compiled_stick_rules().is_some(),
            self.binding.shell().is_some()
        );
        if matches!(self.binding.source(), BindingSource::Blacklisted) {
            self.button_repeats.clear();
        }
    }
}

#[cfg(test)]
mod axis_snapshot_tests {
    use super::*;

    fn controller_info(id: ControllerId) -> ControllerInfo {
        ControllerInfo {
            id,
            name: "test controller".into(),
            supports_rumble: false,
            vendor_id: 1,
            product_id: 1,
        }
    }

    #[test]
    fn authoritative_neutral_snapshot_clears_stale_axis_motion() {
        let mut padjutsu = Padjutsu::new();
        padjutsu.add_controller(controller_info(7));
        padjutsu.on_axis_motion(7, CtrlAxis::LeftX, 0.75);
        assert!(padjutsu.controller_stick_has_axis_activity(
            7,
            StickSide::Left,
            0.05
        ));

        assert!(padjutsu.sync_axis_snapshot(7, [0.0; CtrlAxis::ALL.len()]));
        assert!(!padjutsu.controller_stick_has_axis_activity(
            7,
            StickSide::Left,
            0.05
        ));
        assert!(!padjutsu.sync_axis_snapshot(7, [0.0; CtrlAxis::ALL.len()]));
    }
}
