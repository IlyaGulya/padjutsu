use std::{process::Command, time::Duration};

use colored::Colorize;
use padjutsu_control::{PerformerCmd, PerformerWorker};
use padjutsu_gamepad::ControllerManager;

use padjutsu_workspace::{MouseButton, MouseClickType};

use crate::{app::Effect, print_error, print_info};

const DEFAULT_SHELL: &str = "/bin/zsh";

pub struct ActionRunner<'a> {
    worker: &'a PerformerWorker,
    manager: &'a ControllerManager,
    shell: Option<Box<str>>,
}

impl<'a> ActionRunner<'a> {
    pub fn new(worker: &'a PerformerWorker, manager: &'a ControllerManager) -> Self {
        Self {
            worker,
            manager,
            shell: None,
        }
    }

    fn send(&self, cmd: PerformerCmd) {
        // Non-blocking: if the worker queue is full (worker stuck on a slow
        // CGEventPost), drop the command. For movement/scroll the next tick
        // produces a fresh state; for keys this is rare and acceptable.
        if let Err(e) = self.worker.try_send(cmd) {
            print_error!("performer queue full, dropping command: {e:?}");
        }
    }

    pub fn cancel_mouse_motion(&self) {
        self.worker.cancel_mouse_motion();
    }

    pub fn run_effect(&mut self, effect: Effect) {
        match effect {
            Effect::KeyTap(k) => {
                print_info!("ACTION: KeyTap combo={k:?}");
                self.send(PerformerCmd::KeyTap(k));
            }
            Effect::KeyPress(k) => {
                print_info!("ACTION: KeyPress combo={k:?}");
                self.send(PerformerCmd::KeyPress(k));
            }
            Effect::KeyRelease(k) => {
                print_info!("ACTION: KeyRelease combo={k:?}");
                self.send(PerformerCmd::KeyRelease(k));
            }
            Effect::Macros(m) => {
                print_info!("ACTION: Macros ({} combos)", m.len());
                for k in m.iter() {
                    self.send(PerformerCmd::KeyTap(k.clone()));
                }
            }
            Effect::Shell(ref s) => {
                print_info!("ACTION: Shell cmd={s}");
                let _ = self.run_shell(s);
            }
            Effect::MouseClick { button, click_type } => {
                print_info!(
                    "ACTION: MouseClick button={button:?} click_type={click_type:?}"
                );
                let enigo_button = match button {
                    MouseButton::Left => enigo::Button::Left,
                    MouseButton::Right => enigo::Button::Right,
                    MouseButton::Middle => enigo::Button::Middle,
                };
                let cmd = match click_type {
                    MouseClickType::Click => PerformerCmd::MouseClick(enigo_button),
                    MouseClickType::DoubleClick => {
                        PerformerCmd::MouseDoubleClick(enigo_button)
                    }
                };
                self.send(cmd);
            }
            Effect::MousePress { button } => {
                print_info!("ACTION: MousePress button={button:?}");
                let enigo_button = match button {
                    MouseButton::Left => enigo::Button::Left,
                    MouseButton::Right => enigo::Button::Right,
                    MouseButton::Middle => enigo::Button::Middle,
                };
                self.send(PerformerCmd::MousePress(enigo_button));
            }
            Effect::MouseRelease { button } => {
                print_info!("ACTION: MouseRelease button={button:?}");
                let enigo_button = match button {
                    MouseButton::Left => enigo::Button::Left,
                    MouseButton::Right => enigo::Button::Right,
                    MouseButton::Middle => enigo::Button::Middle,
                };
                self.send(PerformerCmd::MouseRelease(enigo_button));
            }
            Effect::MouseMove { dx, dy } => {
                self.send(PerformerCmd::MouseMove { dx, dy });
            }
            Effect::Scroll { h, v } => {
                if h != 0.0 {
                    self.send(PerformerCmd::ScrollX(h));
                }
                if v != 0.0 {
                    self.send(PerformerCmd::ScrollY(v));
                }
            }
            Effect::TrackpadScroll { h, v } => {
                self.send(PerformerCmd::TrackpadScroll {
                    horizontal: h,
                    vertical: v,
                });
            }
            Effect::Rumble { id, ms } => {
                print_info!("ACTION: Rumble id={id} ms={ms}");
                if let Some(h) = self.manager.controller(id) {
                    let _ = h.rumble(1.0, 1.0, Duration::from_millis(ms as u64));
                }
            }
            #[cfg(target_os = "macos")]
            Effect::RawModifierPress(key) => {
                let keycode = key.keycode();
                print_info!(
                    "ACTION: RawModifierPress key={key:?} keycode=0x{keycode:02x}"
                );
                self.send(PerformerCmd::RawModifierPress(keycode));
            }
            #[cfg(target_os = "macos")]
            Effect::RawModifierRelease(key) => {
                let keycode = key.keycode();
                print_info!(
                    "ACTION: RawModifierRelease key={key:?} keycode=0x{keycode:02x}"
                );
                self.send(PerformerCmd::RawModifierRelease(keycode));
            }
            #[cfg(not(target_os = "macos"))]
            Effect::RawModifierPress(_) | Effect::RawModifierRelease(_) => {
                print_error!("ACTION: RawModifier not supported on this platform");
            }
        }
    }

    fn run_shell(&mut self, cmd: &str) -> Result<(), String> {
        let shell = self.shell.clone().unwrap_or(DEFAULT_SHELL.into());
        match Command::new(shell.into_string().as_str())
            .args(["-c", cmd])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => {
                print_info!("shell command spawned");
                Ok(())
            }
            Err(e) => {
                print_error!("shell command error: {}", e);
                Err(e.to_string())
            }
        }
    }

    pub fn set_shell(&mut self, shell: Option<Box<str>>) {
        self.shell = shell;
    }
}
