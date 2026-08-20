mod app;
mod activity;
mod api;
mod cli;
mod domain;
mod logging;
mod runner;
#[cfg(target_os = "macos")]
mod accessibility;

use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::{process, time::Duration};

use clap::Parser;
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use colored::Colorize;
use crossbeam_channel::{select, unbounded};
use lunchctl::{LaunchAgent, LaunchControllable};
use crate::activity::{Monitor, NotificationListener};

use padjutsu_control::{Performer, PerformerWorker};
use padjutsu_gamepad::{ControllerEvent, ControllerManager, set_realtime_priority};
use padjutsu_workspace::Workspace;

use crate::api::{ApiTransport, Command as ApiCommand, UnixSocket};
use crate::app::Padjutsu;
use crate::cli::{Cli, Command, ControlCommand};
use crate::domain::{
    apply_wake_intents, overdue_wake_event, reduce_event, reschedule_wake,
    DomainControl, DomainEvent, DomainStep, RuntimeMode, RuntimeState, SystemEvent,
    TimerEvent, WakeState,
};
use crate::runner::ActionRunner;

const APP_LABEL: &str = "me.gulya.padjutsu";

fn main() -> process::ExitCode {
    let cli = Cli::parse();
    if cli.command != Command::Observe {
        logging::setup(cli.verbose, cli.no_color);
    }

    let bin_path = std::env::current_exe().unwrap();

    match cli.command {
        Command::Run { workspace } => {
            let workspace_path = resolve_workspace_path(workspace.as_deref());
            match padjutsu_metrics::init_default() {
                Ok(path) => padjutsu_metrics::metric!(
                    "service",
                    "[service-metrics] event=start pid={} version={} interval_s={} recorder={}",
                    std::process::id(),
                    env!("CARGO_PKG_VERSION"),
                    padjutsu_metrics::report_interval().as_secs(),
                    path.display(),
                ),
                Err(error) => {
                    print_error!("failed to start metrics flight recorder: {error}");
                }
            }
            #[cfg(target_os = "macos")]
            {
                let _ = accessibility::request_if_needed();
            }
            run_event_loop(Some(workspace_path));
        }
        Command::Start { workspace } => {
            let workspace_path = resolve_workspace_path(workspace.as_deref());

            #[cfg(target_os = "macos")]
            {
                let _ = accessibility::request_if_needed();
            }

            let mut arguments = vec![bin_path.display().to_string()];
            if cli.verbose {
                arguments.push("--verbose".to_string());
            }
            arguments.push("run".to_string());
            arguments.push("--workspace".to_string());
            arguments.push(workspace_path.display().to_string());

            let agent = LaunchAgent {
                label: APP_LABEL.to_string(),
                program_arguments: arguments,
                standard_out_path: "/tmp/padjutsu.out".to_string(),
                standard_error_path: "/tmp/padjutsu.err".to_string(),
                keep_alive: true,
                run_at_load: true,
            };

            if let Err(e) = agent.write() {
                print_error!("Failed to write agent: {}", e);
                return process::ExitCode::FAILURE;
            }

            match agent.is_running() {
                Ok(true) => {
                    print_info!("Agent is already running");
                }
                Ok(false) => {
                    print_info!("Starting agent");
                    if let Err(e) = agent.bootstrap() {
                        print_error!("Failed to bootstrap agent: {}", e);
                        return process::ExitCode::FAILURE;
                    }
                    print_info!("Agent started");
                }
                Err(e) => {
                    print_error!("Failed to check if agent is running: {}", e);
                    return process::ExitCode::FAILURE;
                }
            }
        }
        Command::Stop => {
            if !LaunchAgent::exists(APP_LABEL) {
                print_error!("Agent does not exist");
                return process::ExitCode::FAILURE;
            }

            let agent = LaunchAgent::from_file(APP_LABEL).unwrap();

            match agent.is_running() {
                Ok(true) => {
                    print_info!("Stopping agent");
                    if let Err(e) = agent.boot_out() {
                        print_error!("Failed to stop agent: {}", e);
                        return process::ExitCode::FAILURE;
                    }
                    print_info!("Agent stopped");
                }
                Ok(false) => {
                    print_info!("Agent is not running");
                }
                Err(e) => {
                    print_error!("Failed to check if agent is running: {}", e);
                    return process::ExitCode::FAILURE;
                }
            }
        }
        Command::Status => {
            if !LaunchAgent::exists(APP_LABEL) {
                print_info!("Agent does not exist");
                return process::ExitCode::FAILURE;
            }

            let agent = LaunchAgent::from_file(APP_LABEL).unwrap();
            match agent.is_running() {
                Ok(true) => {
                    print_info!("Agent is running");
                }
                Ok(false) => {
                    print_info!("Agent is not running");
                }
                Err(e) => {
                    print_error!("Failed to check if agent is running: {}", e);
                    return process::ExitCode::FAILURE;
                }
            }
        }
        Command::Observe => {
            logging::setup(true, cli.no_color);
            #[cfg(target_os = "macos")]
            {
                let _ = accessibility::request_if_needed();
            }
            run_event_loop(None);
        }
        Command::Metrics {
            since_minutes,
            at,
            window_minutes,
            incidents_only,
            mark,
        } => {
            if let Some(marker) = mark {
                match padjutsu_metrics::append_marker(&marker) {
                    Ok(path) => {
                        print_info!("incident marker written to {}", path.display())
                    }
                    Err(error) => {
                        print_error!("failed to write incident marker: {error}");
                        return process::ExitCode::FAILURE;
                    }
                }
            } else if let Err(error) = print_metrics_window(
                since_minutes,
                at.as_deref(),
                window_minutes,
                incidents_only,
            ) {
                print_error!("failed to read metrics: {error}");
                return process::ExitCode::FAILURE;
            }
        }
        Command::Command { workspace, command } => match command {
            ControlCommand::Rumble { id, ms } => {
                let workspace_path = resolve_workspace_path(workspace.as_deref());
                match UnixSocket::new(workspace_path)
                    .send_event(ApiCommand::Rumble { id, ms })
                {
                    Ok(_) => {
                        print_info!("Rumbled controller {:?} for {ms}ms", id);
                    }
                    Err(e) => {
                        print_error!("failed to send rumble command: {e}");
                    }
                };
            }
        },
    }

    process::ExitCode::SUCCESS
}

fn print_metrics_window(
    since_minutes: u64,
    at: Option<&str>,
    window_minutes: u64,
    incidents_only: bool,
) -> Result<(), String> {
    let now_ms = Local::now().timestamp_millis().max(0) as u128;
    let minute_ms = 60_000_u128;
    let (from_ms, to_ms) = if let Some(at) = at {
        let center_ms = parse_metrics_time(at)?;
        let radius = u128::from(window_minutes).saturating_mul(minute_ms);
        (
            center_ms.saturating_sub(radius),
            center_ms.saturating_add(radius),
        )
    } else {
        (
            now_ms
                .saturating_sub(u128::from(since_minutes).saturating_mul(minute_ms)),
            now_ms,
        )
    };
    let path = padjutsu_metrics::RecorderConfig::from_env().path;
    let lines = padjutsu_metrics::read_window(&path, from_ms, to_ms)
        .map_err(|error| error.to_string())?;
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "metrics={} from_unix_ms={} to_unix_ms={} incidents_only={}",
        path.display(),
        from_ms,
        to_ms,
        incidents_only,
    )
    .map_err(|error| error.to_string())?;
    let mut shown = 0_u64;
    for line in lines {
        let incident = padjutsu_metrics::classify_incident(&line);
        if incidents_only && incident.is_none() {
            continue;
        }
        if let Some(cause) = incident {
            write!(stdout, "cause={cause} ").map_err(|error| error.to_string())?;
        }
        writeln!(stdout, "{line}").map_err(|error| error.to_string())?;
        shown += 1;
    }
    writeln!(stdout, "records={shown}").map_err(|error| error.to_string())?;
    Ok(())
}

fn parse_metrics_time(value: &str) -> Result<u128, String> {
    if let Ok(value) = DateTime::parse_from_rfc3339(value) {
        return Ok(value.timestamp_millis().max(0) as u128);
    }
    for format in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        let Ok(naive) = NaiveDateTime::parse_from_str(value, format) else {
            continue;
        };
        let Some(local) = Local.from_local_datetime(&naive).single() else {
            return Err(format!("ambiguous local time: {value}"));
        };
        return Ok(local.timestamp_millis().max(0) as u128);
    }
    Err(format!(
        "unsupported time '{value}'; use 'YYYY-MM-DD HH:MM:SS' or RFC3339"
    ))
}

fn resolve_workspace_path(workspace: Option<&str>) -> PathBuf {
    let workspace = workspace.map(PathBuf::from);
    if let Some(workspace) = workspace {
        // If the user passed a file path (e.g. gc_profile.yaml), use its parent directory
        if workspace.is_file() {
            return workspace.parent().unwrap_or(&workspace).to_owned();
        }
        return workspace;
    }

    match Workspace::default_path() {
        Ok(path) => path,
        Err(e) => {
            print_error!("failed to resolve workspace: {e}");

            process::exit(1);
        }
    }
}

fn run_effects(
    action_runner: &mut ActionRunner<'_>,
    effects: Vec<crate::app::Effect>,
) {
    for effect in effects {
        action_runner.run_effect(effect);
    }
}

fn apply_domain_step(
    step: DomainStep,
    runtime_state: &mut RuntimeState,
    action_runner: &mut ActionRunner<'_>,
    wake_state: &mut WakeState,
) -> DomainControl {
    for update in step.transition.controller_updates {
        match update.next_state {
            Some(state) => runtime_state.set_controller_state(update.id, state),
            None => {
                runtime_state.disconnect_controller(update.id);
            }
        }
    }
    if let Some(crate::domain::ModeTransition::Set(next_mode)) = step.transition.mode
    {
        runtime_state.set_mode(next_mode);
    }
    if let Some(crate::domain::ShellTransition::Set(shell)) = step.transition.shell {
        action_runner.set_shell(shell);
    }
    run_effects(action_runner, step.transition.effects);
    apply_wake_intents(wake_state, step.transition.wake);
    step.control
}

fn process_overdue_wake(
    padjutsu: &mut Padjutsu,
    runtime_state: &mut RuntimeState,
    action_runner: &mut ActionRunner<'_>,
    manager: &ControllerManager,
    wake_state: &mut WakeState,
) -> DomainControl {
    let mouse_was_active = padjutsu.has_active_mouse_axis_input();
    let corrections = sync_latest_controller_axes(padjutsu, manager);
    wake_state.record_axis_snapshot_corrections(corrections);
    let cancellations = sync_latest_button_repeats(padjutsu, manager);
    wake_state.record_button_repeat_snapshot_cancellations(cancellations);
    if mouse_was_active && !padjutsu.has_active_mouse_axis_input() {
        action_runner.cancel_mouse_motion();
    }
    let Some(event) =
        overdue_wake_event(padjutsu, wake_state, std::time::Instant::now())
    else {
        return DomainControl::Continue;
    };

    wake_state.record_timer_wake(std::time::Instant::now());
    let step = reduce_event(event, padjutsu, manager, runtime_state, wake_state);
    apply_domain_step(step, runtime_state, action_runner, wake_state)
}

fn dispatch_domain_event(
    event: DomainEvent,
    padjutsu: &mut Padjutsu,
    runtime_state: &mut RuntimeState,
    action_runner: &mut ActionRunner<'_>,
    manager: &ControllerManager,
    wake_state: &mut WakeState,
) -> DomainControl {
    let mouse_was_active = padjutsu.has_active_mouse_axis_input();
    if matches!(&event, DomainEvent::Timer(TimerEvent::Wake)) {
        let corrections = sync_latest_controller_axes(padjutsu, manager);
        wake_state.record_axis_snapshot_corrections(corrections);
        let cancellations = sync_latest_button_repeats(padjutsu, manager);
        wake_state.record_button_repeat_snapshot_cancellations(cancellations);
        wake_state.record_timer_wake(std::time::Instant::now());
    }
    let step = reduce_event(event, padjutsu, manager, runtime_state, wake_state);
    if mouse_was_active && !padjutsu.has_active_mouse_axis_input() {
        action_runner.cancel_mouse_motion();
    }
    apply_domain_step(step, runtime_state, action_runner, wake_state)
}

fn sync_latest_controller_axes(
    padjutsu: &mut Padjutsu,
    manager: &ControllerManager,
) -> u64 {
    let mut corrections = 0_u64;
    manager.for_each_axis_snapshot(|id, axes| {
        corrections += u64::from(padjutsu.sync_axis_snapshot(id, axes));
    });
    if corrections > 0 {
        print_debug!(
            "latest-axis snapshot corrected {corrections} queued controller states"
        );
    }
    corrections
}

fn sync_latest_button_repeats(
    padjutsu: &mut Padjutsu,
    manager: &ControllerManager,
) -> u64 {
    let mut cancellations = 0_u64;
    manager.for_each_button_snapshot(|id, buttons| {
        cancellations += padjutsu.sync_button_repeat_snapshot(id, buttons);
    });
    if cancellations > 0 {
        print_debug!(
            "latest-button snapshot cancelled {cancellations} stale repeats"
        );
    }
    cancellations
}

fn dispatch_and_process_overdue(
    event: DomainEvent,
    padjutsu: &mut Padjutsu,
    runtime_state: &mut RuntimeState,
    action_runner: &mut ActionRunner<'_>,
    manager: &ControllerManager,
    wake_state: &mut WakeState,
) -> DomainControl {
    let control = dispatch_domain_event(
        event,
        padjutsu,
        runtime_state,
        action_runner,
        manager,
        wake_state,
    );
    if let DomainControl::Break = control {
        return DomainControl::Break;
    }
    process_overdue_wake(padjutsu, runtime_state, action_runner, manager, wake_state)
}

fn run_event_loop(maybe_workspace_path: Option<PathBuf>) {
    // Ensure only one instance runs per workspace by holding an exclusive flock.
    if let Some(ws) = maybe_workspace_path.as_ref() {
        let lock_path = ws.parent().unwrap_or(ws).join("padjutsud.lock");
        let file = File::create(&lock_path).unwrap_or_else(|e| {
            print_error!("failed to create lock file {}: {e}", lock_path.display());
            process::exit(1);
        });
        let mut lock = fd_lock::RwLock::new(file);
        match lock.try_write() {
            Ok(guard) => {
                // Forget the guard so Drop doesn't unlock.
                // The fd stays open via `lock` (kept alive below), so the OS lock persists
                // until the process exits.
                std::mem::forget(guard);
            }
            Err(_) => {
                print_error!(
                    "another padjutsud is already running (lock: {})",
                    lock_path.display()
                );
                process::exit(1);
            }
        }
        // Leak the RwLock to keep the fd open for the process lifetime.
        std::mem::forget(lock);
    }

    #[cfg(target_os = "macos")]
    {
        let trusted = accessibility::is_trusted();
        print_info!("accessibility trusted: {trusted}");
        if !trusted {
            accessibility::log_missing_permission();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let trusted = accessibility::is_trusted();
                if trusted {
                    print_info!("accessibility trusted: {trusted}");
                    break;
                }
            }
        }
    }

    // Activity monitor must run on the main thread.
    // We keep its std::mpsc receiver and poll it from the event loop (no bridge thread).
    let Some((monitor, activity_std_rx, monitor_stop_tx)) = Monitor::new() else {
        print_error!("failed to start activity monitor");
        return;
    };

    monitor.subscribe(NotificationListener::DidActivateApplication);
    let mut padjutsu = Padjutsu::new();
    if let Some(app) = monitor.get_active_application() {
        padjutsu.set_active_app(&app)
    }

    // Handle Ctrl+C to exit cleanly
    let (stop_tx, stop_rx) = unbounded::<()>();
    ctrlc::set_handler(move || {
        let _ = stop_tx.send(());
        let _ = monitor_stop_tx.send(());
    })
    .expect("failed to set Ctrl+C handler");

    let workspace_path = maybe_workspace_path.to_owned();

    // Start control socket on the main thread and forward commands into the event loop.
    let (api_tx, api_rx) = unbounded::<ApiCommand>();
    let _control_handle = workspace_path.clone().and_then(|ws_path| {
        let sock_dir = ws_path.parent().unwrap_or(&ws_path).to_owned();
        match UnixSocket::new(sock_dir).listen_events(api_tx) {
            Ok(h) => Some(h),
            Err(e) => {
                print_error!("failed to start api server: {e}");
                None
            }
        }
    });

    // Run the main event loop in a background thread while the main thread runs the monitor loop.
    let event_loop = std::thread::Builder::new()
        .name("event-loop".into())
        .stack_size(512 * 1024)
        .spawn(move || {
            set_realtime_priority();

            let manager = ControllerManager::new()
                .expect("failed to start controller manager");
            let rx = manager.subscribe();
            let keypress = Performer::new().expect("failed to start keypress");
            let worker = PerformerWorker::spawn(keypress);
            // Single coalesced wake timer: earliest of movement tick and repeat deadlines.
            let mut wake_rx = crossbeam_channel::never::<std::time::Instant>();
            let idle_period = Duration::from_millis(16);
            let fast_period = Duration::from_millis(5);
            let mut runtime_state = RuntimeState::new(RuntimeMode::Booting);
            let mut wake_state = WakeState::new(std::time::Instant::now());

            let workspace = match Workspace::new(workspace_path.as_deref()) {
                Ok(workspace) => workspace,
                Err(e) => {
                    print_error!("failed to start workspace: {e}");
                    return;
                }
            };

            let maybe_watcher = workspace_path
                .as_ref()
                .map(|_| workspace.start_profile_watcher())
                .transpose()
                .expect("failed to start workspace watcher");

            let (maybe_workspace_rx, _profile_watcher) = match maybe_watcher {
                Some((watcher, rx)) => (Some(rx), Some(watcher)),
                None => (None, None),
            };

            let mut action_runner = ActionRunner::new(&worker, &manager);
            runtime_state.set_mode(if padjutsu.workspace.is_some() {
                RuntimeMode::Active
            } else {
                RuntimeMode::AwaitingProfile
            });

            for info in manager.controllers() {
                if let DomainControl::Break = dispatch_domain_event(
                    DomainEvent::Controller(ControllerEvent::Connected(info)),
                    &mut padjutsu,
                    &mut runtime_state,
                    &mut action_runner,
                    &manager,
                    &mut wake_state,
                ) {
                    break;
                }
            }

            print_info!(
                "padjutsud started. Listening for controller and activity events."
            );
            loop {
                select! {
                recv(stop_rx) -> _ => {
                        if let DomainControl::Break = dispatch_domain_event(
                            DomainEvent::System(SystemEvent::ShutdownRequested),
                            &mut padjutsu,
                            &mut runtime_state,
                            &mut action_runner,
                            &manager,
                            &mut wake_state,
                        ) {
                            break;
                        }
                }
                recv(rx) -> msg => {
                    match msg {
                        Ok(event) => {
                            if let DomainControl::Break = dispatch_and_process_overdue(
                                DomainEvent::Controller(event),
                                &mut padjutsu,
                                &mut runtime_state,
                                &mut action_runner,
                                &manager,
                                &mut wake_state,
                            ) {
                                break;
                            }
                            // Drain any additional events that have accumulated
                            // in the channel to prevent unbounded growth when
                            // the producer (SDL2 thread) outpaces the consumer.
                            while let Ok(event) = rx.try_recv() {
                                if let DomainControl::Break = dispatch_and_process_overdue(
                                    DomainEvent::Controller(event),
                                    &mut padjutsu,
                                    &mut runtime_state,
                                    &mut action_runner,
                                    &manager,
                                    &mut wake_state,
                                ) {
                                    break;
                                }
                            }
                        }
                        Err(err) => {
                            print_error!("event channel closed: {err}");
                                break;
                            }
                        }
                    }
                recv(api_rx) -> cmd => {
                        if let Ok(command) = cmd {
                            if let DomainControl::Break = dispatch_domain_event(
                                DomainEvent::Api(command),
                                &mut padjutsu,
                                &mut runtime_state,
                                &mut action_runner,
                                &manager,
                                &mut wake_state,
                            ) {
                                break;
                            }
                        }
                }
                recv(wake_rx) -> _ => {
                    if let DomainControl::Break = dispatch_domain_event(
                        DomainEvent::Timer(TimerEvent::Wake),
                        &mut padjutsu,
                        &mut runtime_state,
                        &mut action_runner,
                        &manager,
                        &mut wake_state,
                    ) {
                        break;
                    }
                }
            }
            while let Ok(msg) = activity_std_rx.try_recv() {
                if let DomainControl::Break = dispatch_and_process_overdue(
                    DomainEvent::Activity(msg),
                    &mut padjutsu,
                    &mut runtime_state,
                    &mut action_runner,
                    &manager,
                    &mut wake_state,
                ) {
                    break;
                }
            }
                let Some(workspace_rx) = maybe_workspace_rx.as_ref() else {
                    continue;
                };

                while let Ok(msg) = workspace_rx.try_recv() {
                if let DomainControl::Break = dispatch_and_process_overdue(
                    DomainEvent::Profile(msg),
                    &mut padjutsu,
                    &mut runtime_state,
                    &mut action_runner,
                    &manager,
                    &mut wake_state,
                ) {
                    break;
                }
                }
                if wake_state.need_reschedule {
                    let now = std::time::Instant::now();
                    let wake_plan = reschedule_wake(
                        &padjutsu,
                        &mut wake_state,
                        idle_period,
                        fast_period,
                    );
                    if let Some(due) = wake_plan.next_due {
                        let dur = if due > now { due - now } else { Duration::ZERO };
                        print_debug!(
                            "wake arm: next_due={due:?} in_ms={} tick_due={:?} stick_repeat_due={:?} button_repeat_due={:?}",
                            dur.as_millis(),
                            wake_state.next_tick_due,
                            wake_plan.repeat_due,
                            wake_plan.button_repeat_due
                        );
                        wake_rx = crossbeam_channel::after(dur);
                    } else {
                        print_debug!("wake arm: no pending deadlines");
                        wake_rx = crossbeam_channel::never();
                    }
                    wake_state.need_reschedule = false;
                }
            }
        })
        .expect("failed to spawn event loop thread");

    // Start monitoring on the main thread (blocks until error/exit)
    monitor.run();
    if let Err(e) = event_loop.join() {
        print_error!("event loop error: {e:?}");
    }
}
