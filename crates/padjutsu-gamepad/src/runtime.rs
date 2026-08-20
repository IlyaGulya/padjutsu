use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use ahash::{AHashMap, AHashSet};
use sdl2::controller::{Button as SdlButton, GameController, Axis as SdlAxis};
use sdl2::event::Event;
use sdl2::haptic::Haptic;
use sdl2::joystick::Joystick;

use crate::command::Command;
use crate::events::ControllerEvent;
use crate::manager::Inner;
use crate::metrics::Metrics;
use crate::types::{Axis, AxisSnapshot, Button, ControllerId, ControllerInfo};

const AXIS_POLL_INTERVAL: Duration = Duration::from_millis(8);
const SDL_AXES: [SdlAxis; 6] = [
    SdlAxis::LeftX,
    SdlAxis::LeftY,
    SdlAxis::RightX,
    SdlAxis::RightY,
    SdlAxis::TriggerLeft,
    SdlAxis::TriggerRight,
];

// --- Mach real-time thread priority via raw FFI ---

#[cfg(target_os = "macos")]
mod mach_rt {
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
        preemptible: u32, // boolean_t is typedef'd as int on macOS
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

    /// Convert nanoseconds to Mach absolute time units.
    fn ns_to_abs(ns: u64) -> u32 {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        unsafe {
            mach_timebase_info(&mut info);
        }
        // abs_time = ns * denom / numer
        (ns * (info.denom as u64) / (info.numer as u64)) as u32
    }

    pub fn set_realtime_priority_impl() {
        // Parameters for a 2ms input processing loop:
        //   period:      2ms   (how often we need CPU)
        //   computation: 500us (how much CPU per period)
        //   constraint:  1ms   (deadline within period)
        //   preemptible: true
        let period_ns: u64 = 2_000_000; // 2ms
        let computation_ns: u64 = 500_000; // 500us
        let constraint_ns: u64 = 1_000_000; // 1ms

        let policy = ThreadTimeConstraintPolicy {
            period: ns_to_abs(period_ns),
            computation: ns_to_abs(computation_ns),
            constraint: ns_to_abs(constraint_ns),
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
            eprintln!("[padjutsu-gamepad] real-time thread priority set (period=2ms computation=500us constraint=1ms)");
            padjutsu_metrics::metric!(
                "thread-policy",
                "[thread-policy-metrics] name={} requested=time_constraint result=success",
                std::thread::current().name().unwrap_or("gamepad-runtime"),
            );
        } else {
            eprintln!("[padjutsu-gamepad] WARNING: failed to set real-time thread priority (kern_return={})", kr);
            padjutsu_metrics::metric!(
                "thread-policy",
                "[thread-policy-metrics] name={} requested=time_constraint result=failure kern_return={}",
                std::thread::current().name().unwrap_or("gamepad-runtime"),
                kr,
            );
        }
    }
}

/// Set the calling thread to macOS real-time priority using `THREAD_TIME_CONSTRAINT_POLICY`.
///
/// This is the proper macOS real-time API used by Core Audio -- it guarantees CPU time
/// even under system load without causing priority inversion with WindowServer.
///
/// Parameters are tuned for a 2ms input processing loop:
/// - period: 2ms (how often we need CPU)
/// - computation: 500us (how much CPU per period)
/// - constraint: 1ms (deadline within period)
/// - preemptible: true
///
/// On non-macOS platforms this is a no-op.
pub fn set_realtime_priority() {
    #[cfg(target_os = "macos")]
    mach_rt::set_realtime_priority_impl();
}

/// Starts the SDL2-backed runtime thread that drives device discovery and events.
pub(crate) fn start_runtime_thread(
    inner: Arc<Inner>,
    cmd_rx: Receiver<Command>,
    ready_tx: Option<std::sync::mpsc::Sender<()>>,
) {
    thread::spawn(move || {
        set_native_thread_name();
        set_realtime_priority();
        crate::metrics::init();

        // SDL must live entirely within this thread
        let sdl_ctx = match sdl2::init() {
            Ok(ctx) => ctx,
            Err(_) => {
                return;
            }
        };
        let controller_subsystem = match sdl_ctx.game_controller() {
            Ok(c) => c,
            Err(_) => return,
        };
        let joystick_subsystem = match sdl_ctx.joystick() {
            Ok(j) => j,
            Err(_) => return,
        };
        let haptic_subsystem = match sdl_ctx.haptic() {
            Ok(h) => h,
            Err(_) => return,
        };
        let mut event_pump = match sdl_ctx.event_pump() {
            Ok(p) => p,
            Err(_) => return,
        };

        let mut controllers: AHashMap<ControllerId, GameController> =
            AHashMap::new();
        let mut joysticks: AHashMap<ControllerId, Joystick> = AHashMap::new();
        let mut haptics: AHashMap<ControllerId, Haptic> = AHashMap::new();
        let mut trigger_state: AHashMap<ControllerId, (bool, bool)> =
            AHashMap::new();
        let mut button_state: AHashMap<ControllerId, AHashSet<Button>> =
            AHashMap::new();
        let mut last_axis_poll = Instant::now() - AXIS_POLL_INTERVAL;

        // Initial enumeration
        if let Ok(num_joysticks) = joystick_subsystem.num_joysticks() {
            for i in 0..num_joysticks {
                if controller_subsystem.is_game_controller(i) {
                    if let Ok(controller) = controller_subsystem.open(i) {
                        let id: ControllerId = match joystick_subsystem.open(i) {
                            Ok(js) => js.instance_id() as ControllerId,
                            Err(_) => i as ControllerId,
                        };
                        let info = ControllerInfo {
                            id,
                            name: controller.name().to_string(),
                            vendor_id: controller.vendor_id().unwrap_or(0),
                            product_id: controller.product_id().unwrap_or(0),
                            supports_rumble: controller.has_rumble(),
                        };
                        controllers.insert(id, controller);
                        if let Ok(mut map) = inner.controllers_info.write() {
                            map.insert(id, info.clone());
                        }
                        broadcast(&inner, ControllerEvent::Connected(info));
                    }
                } else if let Ok(joystick) = joystick_subsystem.open(i) {
                    let id: ControllerId = joystick.instance_id() as ControllerId;
                    if joystick.has_rumble() {
                        if let Ok(h) = haptic_subsystem
                            .open_from_joystick_id(joystick.instance_id())
                        {
                            haptics.insert(id, h);
                        }
                    }
                    let info = ControllerInfo {
                        id,
                        name: joystick.name().to_string(),
                        vendor_id: 0,
                        product_id: 0,
                        supports_rumble: joystick.has_rumble(),
                    };
                    joysticks.insert(id, joystick);
                    if let Ok(mut map) = inner.controllers_info.write() {
                        map.insert(id, info.clone());
                    }
                    broadcast(&inner, ControllerEvent::Connected(info));
                }
            }
        }

        if let Some(tx) = ready_tx {
            let _ = tx.send(());
        }

        loop {
            // Wait for an SDL event or timeout to reduce idle CPU usage
            if let Some(event) = event_pump.wait_event_timeout(1) {
                match event {
                    Event::ControllerDeviceAdded { which, .. } => {
                        if let Ok(controller) = controller_subsystem.open(which) {
                            let id: ControllerId =
                                match joystick_subsystem.open(which) {
                                    Ok(js) => js.instance_id() as ControllerId,
                                    Err(_) => which as ControllerId,
                                };
                            let info = ControllerInfo {
                                id,
                                name: controller.name().to_string(),
                                vendor_id: controller.vendor_id().unwrap_or(0),
                                product_id: controller.product_id().unwrap_or(0),
                                supports_rumble: controller.has_rumble(),
                            };
                            controllers.insert(id, controller);
                            if let Ok(mut map) = inner.controllers_info.write() {
                                map.insert(id, info.clone());
                            }
                            broadcast(&inner, ControllerEvent::Connected(info));
                        }
                    }
                    Event::ControllerDeviceRemoved { which, .. } => {
                        let id: ControllerId = which as ControllerId;
                        controllers.remove(&id);
                        joysticks.remove(&id);
                        haptics.remove(&id);
                        trigger_state.remove(&id);
                        button_state.remove(&id);
                        if let Ok(mut map) = inner.controllers_info.write() {
                            map.remove(&id);
                        }
                        broadcast(&inner, ControllerEvent::Disconnected(id));
                    }
                    Event::ControllerButtonDown { which, button, .. } => {
                        if let Some(btn) = map_sdl_button(button) {
                            let id = which as ControllerId;
                            let emitted =
                                button_state.entry(id).or_default().insert(btn);
                            record_raw_button_metric(true, emitted);
                            if emitted {
                                broadcast(
                                    &inner,
                                    ControllerEvent::ButtonPressed {
                                        id,
                                        button: btn,
                                    },
                                );
                            }
                        }
                    }
                    Event::ControllerButtonUp { which, button, .. } => {
                        if let Some(btn) = map_sdl_button(button) {
                            let id = which as ControllerId;
                            let emitted = button_state
                                .get_mut(&id)
                                .is_some_and(|s| s.remove(&btn));
                            record_raw_button_metric(false, emitted);
                            if emitted {
                                broadcast(
                                    &inner,
                                    ControllerEvent::ButtonReleased {
                                        id,
                                        button: btn,
                                    },
                                );
                            }
                        }
                    }
                    Event::ControllerAxisMotion {
                        which, axis, value, ..
                    } => {
                        const THRESHOLD: i16 = 20000;
                        let id = which as ControllerId;
                        let entry =
                            trigger_state.entry(id).or_insert((false, false));

                        // Emit analog event for all axes
                        if let Some(mapped) = map_sdl_axis(axis) {
                            let norm = (value as f32) / (i16::MAX as f32);
                            broadcast(
                                &inner,
                                ControllerEvent::AxisMotion {
                                    id,
                                    axis: mapped,
                                    value: norm,
                                },
                            );
                        }

                        // Preserve trigger-as-button semantics for compatibility
                        match axis {
                            SdlAxis::TriggerLeft => {
                                let pressed = value > THRESHOLD;
                                if pressed && !entry.0 {
                                    broadcast(
                                        &inner,
                                        ControllerEvent::ButtonPressed {
                                            id,
                                            button: Button::LeftTrigger,
                                        },
                                    );
                                    entry.0 = true;
                                } else if !pressed && entry.0 {
                                    broadcast(
                                        &inner,
                                        ControllerEvent::ButtonReleased {
                                            id,
                                            button: Button::LeftTrigger,
                                        },
                                    );
                                    entry.0 = false;
                                }
                            }
                            SdlAxis::TriggerRight => {
                                let pressed = value > THRESHOLD;
                                if pressed && !entry.1 {
                                    broadcast(
                                        &inner,
                                        ControllerEvent::ButtonPressed {
                                            id,
                                            button: Button::RightTrigger,
                                        },
                                    );
                                    entry.1 = true;
                                } else if !pressed && entry.1 {
                                    broadcast(
                                        &inner,
                                        ControllerEvent::ButtonReleased {
                                            id,
                                            button: Button::RightTrigger,
                                        },
                                    );
                                    entry.1 = false;
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
                // Drain any additional queued events quickly
                for ev in event_pump.poll_iter() {
                    match ev {
                        Event::ControllerDeviceAdded { which, .. } => {
                            if let Ok(controller) = controller_subsystem.open(which)
                            {
                                let id: ControllerId =
                                    match joystick_subsystem.open(which) {
                                        Ok(js) => js.instance_id() as ControllerId,
                                        Err(_) => which as ControllerId,
                                    };
                                let info = ControllerInfo {
                                    id,
                                    name: controller.name().to_string(),
                                    vendor_id: controller.vendor_id().unwrap_or(0),
                                    product_id: controller.product_id().unwrap_or(0),
                                    supports_rumble: controller.has_rumble(),
                                };
                                controllers.insert(id, controller);
                                if let Ok(mut map) = inner.controllers_info.write() {
                                    map.insert(id, info.clone());
                                }
                                broadcast(&inner, ControllerEvent::Connected(info));
                            }
                        }
                        Event::ControllerDeviceRemoved { which, .. } => {
                            let id: ControllerId = which as ControllerId;
                            controllers.remove(&id);
                            joysticks.remove(&id);
                            haptics.remove(&id);
                            trigger_state.remove(&id);
                            button_state.remove(&id);
                            if let Ok(mut map) = inner.controllers_info.write() {
                                map.remove(&id);
                            }
                            broadcast(&inner, ControllerEvent::Disconnected(id));
                        }
                        Event::ControllerButtonDown { which, button, .. } => {
                            if let Some(btn) = map_sdl_button(button) {
                                let id = which as ControllerId;
                                let emitted =
                                    button_state.entry(id).or_default().insert(btn);
                                record_raw_button_metric(true, emitted);
                                if emitted {
                                    broadcast(
                                        &inner,
                                        ControllerEvent::ButtonPressed {
                                            id,
                                            button: btn,
                                        },
                                    );
                                }
                            }
                        }
                        Event::ControllerButtonUp { which, button, .. } => {
                            if let Some(btn) = map_sdl_button(button) {
                                let id = which as ControllerId;
                                let emitted = button_state
                                    .get_mut(&id)
                                    .is_some_and(|s| s.remove(&btn));
                                record_raw_button_metric(false, emitted);
                                if emitted {
                                    broadcast(
                                        &inner,
                                        ControllerEvent::ButtonReleased {
                                            id,
                                            button: btn,
                                        },
                                    );
                                }
                            }
                        }
                        Event::ControllerAxisMotion {
                            which, axis, value, ..
                        } => {
                            const THRESHOLD: i16 = 20000;
                            let id = which as ControllerId;
                            let entry =
                                trigger_state.entry(id).or_insert((false, false));
                            if let Some(mapped) = map_sdl_axis(axis) {
                                let norm = (value as f32) / (i16::MAX as f32);
                                broadcast(
                                    &inner,
                                    ControllerEvent::AxisMotion {
                                        id,
                                        axis: mapped,
                                        value: norm,
                                    },
                                );
                            }
                            match axis {
                                SdlAxis::TriggerLeft => {
                                    let pressed = value > THRESHOLD;
                                    if pressed && !entry.0 {
                                        broadcast(
                                            &inner,
                                            ControllerEvent::ButtonPressed {
                                                id,
                                                button: Button::LeftTrigger,
                                            },
                                        );
                                        entry.0 = true;
                                    } else if !pressed && entry.0 {
                                        broadcast(
                                            &inner,
                                            ControllerEvent::ButtonReleased {
                                                id,
                                                button: Button::LeftTrigger,
                                            },
                                        );
                                        entry.0 = false;
                                    }
                                }
                                SdlAxis::TriggerRight => {
                                    let pressed = value > THRESHOLD;
                                    if pressed && !entry.1 {
                                        broadcast(
                                            &inner,
                                            ControllerEvent::ButtonPressed {
                                                id,
                                                button: Button::RightTrigger,
                                            },
                                        );
                                        entry.1 = true;
                                    } else if !pressed && entry.1 {
                                        broadcast(
                                            &inner,
                                            ControllerEvent::ButtonReleased {
                                                id,
                                                button: Button::RightTrigger,
                                            },
                                        );
                                        entry.1 = false;
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Handle commands
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    Command::Rumble { id, low, high, ms } => {
                        if let Some(ctrl) = controllers.get_mut(&id) {
                            if let Err(e) = ctrl.set_rumble(low, high, ms) {
                                println!("{}", ctrl.has_rumble());
                                eprintln!("Failed to set rumble: {e}");
                            }
                        } else if let Some(h) = haptics.get_mut(&id) {
                            let strength = (low.max(high) as f32) / 65535.0;
                            h.rumble_play(strength, ms);
                        }
                    }
                    Command::StopRumble { id } => {
                        if let Some(ctrl) = controllers.get_mut(&id) {
                            if let Err(e) = ctrl.set_rumble(0, 0, 0) {
                                eprintln!("Failed to stop rumble: {e}");
                            }
                        } else if let Some(h) = haptics.get_mut(&id) {
                            h.rumble_stop();
                        }
                    }
                }
            }

            poll_axis_snapshots(
                &inner,
                &controllers,
                &mut last_axis_poll,
                Instant::now(),
            );

            metrics_tick();
        }
    });
}

#[cfg(target_os = "macos")]
fn set_native_thread_name() {
    use std::os::raw::{c_char, c_int};

    unsafe extern "C" {
        fn pthread_setname_np(name: *const c_char) -> c_int;
    }
    let _ = unsafe { pthread_setname_np(c"gamepad-runtime".as_ptr()) };
}

#[cfg(not(target_os = "macos"))]
fn set_native_thread_name() {}

fn map_sdl_button(button: SdlButton) -> Option<Button> {
    Some(match button {
        SdlButton::A => Button::A,
        SdlButton::B => Button::B,
        SdlButton::X => Button::X,
        SdlButton::Y => Button::Y,
        SdlButton::Back => Button::Back,
        SdlButton::Guide => Button::Guide,
        SdlButton::Start => Button::Start,
        SdlButton::LeftStick => Button::LeftStick,
        SdlButton::RightStick => Button::RightStick,
        SdlButton::LeftShoulder => Button::LeftShoulder,
        SdlButton::RightShoulder => Button::RightShoulder,
        SdlButton::DPadUp => Button::DPadUp,
        SdlButton::DPadDown => Button::DPadDown,
        SdlButton::DPadLeft => Button::DPadLeft,
        SdlButton::DPadRight => Button::DPadRight,
        _ => return None,
    })
}

fn map_sdl_axis(axis: SdlAxis) -> Option<Axis> {
    Some(match axis {
        SdlAxis::LeftX => Axis::LeftX,
        SdlAxis::LeftY => Axis::LeftY,
        SdlAxis::RightX => Axis::RightX,
        SdlAxis::RightY => Axis::RightY,
        SdlAxis::TriggerLeft => Axis::LeftTrigger,
        SdlAxis::TriggerRight => Axis::RightTrigger,
    })
}

fn poll_axis_snapshots(
    inner: &Inner,
    controllers: &AHashMap<ControllerId, GameController>,
    last_poll: &mut Instant,
    now: Instant,
) {
    if now.saturating_duration_since(*last_poll) < AXIS_POLL_INTERVAL {
        return;
    }
    *last_poll = now;

    let polled: Vec<(ControllerId, AxisSnapshot)> = controllers
        .iter()
        .map(|(id, controller)| {
            let mut axes = [0.0; Axis::ALL.len()];
            for (index, axis) in SDL_AXES.iter().copied().enumerate() {
                axes[index] = f32::from(controller.axis(axis)) / f32::from(i16::MAX);
            }
            (*id, axes)
        })
        .collect();

    if let Ok(mut snapshots) = inner.controller_axes.write() {
        for (id, axes) in polled {
            snapshots.insert(id, axes);
        }
    }
}

thread_local! {
    static METRICS: std::cell::RefCell<Metrics> = std::cell::RefCell::new(Metrics::default());
}

fn broadcast(inner: &Inner, event: ControllerEvent) {
    use crossbeam_channel::TrySendError;
    if let Ok(mut snapshots) = inner.controller_axes.write() {
        match &event {
            ControllerEvent::Connected(info) => {
                snapshots.entry(info.id).or_insert([0.0; Axis::ALL.len()]);
            }
            ControllerEvent::Disconnected(id) => {
                snapshots.remove(id);
            }
            ControllerEvent::AxisMotion { id, axis, value } => {
                snapshots.entry(*id).or_insert([0.0; Axis::ALL.len()])
                    [axis.index()] = *value;
            }
            ControllerEvent::ButtonPressed { .. }
            | ControllerEvent::ButtonReleased { .. } => {}
        }
    }
    if let Ok(mut snapshots) = inner.controller_buttons.write() {
        match &event {
            ControllerEvent::Connected(info) => {
                snapshots
                    .entry(info.id)
                    .or_insert([false; Button::ALL.len()]);
            }
            ControllerEvent::Disconnected(id) => {
                snapshots.remove(id);
            }
            ControllerEvent::ButtonPressed { id, button } => {
                snapshots.entry(*id).or_insert([false; Button::ALL.len()])
                    [button.index()] = true;
            }
            ControllerEvent::ButtonReleased { id, button } => {
                snapshots.entry(*id).or_insert([false; Button::ALL.len()])
                    [button.index()] = false;
            }
            ControllerEvent::AxisMotion { .. } => {}
        }
    }
    if crate::metrics::is_enabled() {
        METRICS.with(|m| {
            let mut m = m.borrow_mut();
            match &event {
                ControllerEvent::AxisMotion { axis, .. } => {
                    m.record_axis(*axis, Instant::now())
                }
                ControllerEvent::ButtonPressed { .. }
                | ControllerEvent::ButtonReleased { .. } => m.record_button(),
                _ => {}
            }
        });
    }
    let t0 = if crate::metrics::is_enabled() {
        Some(Instant::now())
    } else {
        None
    };
    let mut subscriber_drops = 0;
    if let Ok(mut subs) = inner.subscribers.lock() {
        subs.retain(|tx| match tx.try_send(event.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                subscriber_drops += 1;
                true
            }
            Err(TrySendError::Disconnected(_)) => false, // remove subscriber
        });
    }
    if subscriber_drops > 0 {
        METRICS.with(|m| m.borrow_mut().record_subscriber_drops(subscriber_drops));
    }
    if let Some(t0) = t0 {
        let cost = Instant::now().saturating_duration_since(t0);
        METRICS.with(|m| m.borrow_mut().record_broadcast_cost(cost));
    }
}

/// Periodic metrics flush — called from the SDL event loop on every iteration.
fn metrics_tick() {
    if crate::metrics::is_enabled() {
        METRICS.with(|m| {
            let mut metrics = m.borrow_mut();
            metrics.record_loop_tick(Instant::now());
            metrics.maybe_report();
        });
    }
}

fn record_raw_button_metric(pressed: bool, emitted: bool) {
    if crate::metrics::is_enabled() {
        METRICS.with(|metrics| {
            metrics.borrow_mut().record_raw_button(pressed, emitted)
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, RwLock};

    use crossbeam_channel::{bounded, unbounded};

    use super::*;

    #[test]
    fn latest_axis_snapshot_bypasses_a_full_subscriber_queue() {
        let (cmd_tx, _cmd_rx) = unbounded();
        let (subscriber_tx, _subscriber_rx) = bounded(0);
        let inner = Inner {
            subscribers: Mutex::new(vec![subscriber_tx]),
            controllers_info: RwLock::new(AHashMap::new()),
            controller_axes: RwLock::new(AHashMap::new()),
            controller_buttons: RwLock::new(AHashMap::new()),
            cmd_tx,
        };

        broadcast(
            &inner,
            ControllerEvent::AxisMotion {
                id: 7,
                axis: Axis::LeftX,
                value: 0.75,
            },
        );

        let snapshots = inner.controller_axes.read().unwrap();
        assert_eq!(snapshots[&7][Axis::LeftX.index()], 0.75);
    }

    #[test]
    fn latest_button_snapshot_bypasses_a_full_subscriber_queue() {
        let (cmd_tx, _cmd_rx) = unbounded();
        let (subscriber_tx, _subscriber_rx) = bounded(0);
        let inner = Inner {
            subscribers: Mutex::new(vec![subscriber_tx]),
            controllers_info: RwLock::new(AHashMap::new()),
            controller_axes: RwLock::new(AHashMap::new()),
            controller_buttons: RwLock::new(AHashMap::new()),
            cmd_tx,
        };

        broadcast(
            &inner,
            ControllerEvent::ButtonPressed {
                id: 7,
                button: Button::A,
            },
        );
        broadcast(
            &inner,
            ControllerEvent::ButtonReleased {
                id: 7,
                button: Button::A,
            },
        );

        let snapshots = inner.controller_buttons.read().unwrap();
        assert!(!snapshots[&7][Button::A.index()]);
    }
}
