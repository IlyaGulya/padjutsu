use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const STALL_TIMEOUT: Duration = Duration::from_secs(5);
const CHECK_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) fn start(progress: Arc<AtomicU64>) {
    thread::Builder::new()
        .name("gamepad-watchdog".into())
        .spawn(move || {
            let mut watchdog = ProgressWatchdog::new(
                progress.load(Ordering::Relaxed),
                Instant::now(),
            );
            loop {
                thread::sleep(CHECK_INTERVAL);
                let observed = progress.load(Ordering::Relaxed);
                if !watchdog.stalled(observed, Instant::now()) {
                    continue;
                }

                // Confirm once more so a delayed wake after system sleep does not
                // mistake a momentary scheduling delay for a blocked SDL call.
                thread::sleep(CHECK_INTERVAL);
                if !watchdog.stalled(progress.load(Ordering::Relaxed), Instant::now()) {
                    continue;
                }

                let message = format!(
                    "[gamepad-watchdog] SDL runtime made no progress for at least {}s; exiting for launchd recovery",
                    STALL_TIMEOUT.as_secs(),
                );
                eprintln!("{message}");
                if let Err(error) = padjutsu_metrics::append_marker(&message) {
                    eprintln!("[gamepad-watchdog] failed to persist incident: {error}");
                }
                std::process::exit(75);
            }
        })
        .expect("failed to spawn gamepad watchdog");
}

pub(crate) struct ProgressWatchdog {
    last_progress: u64,
    last_changed_at: Instant,
}

impl ProgressWatchdog {
    pub(crate) fn new(progress: u64, now: Instant) -> Self {
        Self {
            last_progress: progress,
            last_changed_at: now,
        }
    }

    pub(crate) fn stalled(&mut self, progress: u64, now: Instant) -> bool {
        if progress != self.last_progress {
            self.last_progress = progress;
            self.last_changed_at = now;
            return false;
        }
        now.duration_since(self.last_changed_at) >= STALL_TIMEOUT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_stall_when_the_runtime_stops_completing_loops() {
        let start = Instant::now();
        let mut watchdog = ProgressWatchdog::new(12, start);

        assert!(!watchdog.stalled(12, start + Duration::from_secs(4)));
        assert!(watchdog.stalled(12, start + Duration::from_secs(5)));
    }

    #[test]
    fn completed_loop_resets_the_stall_deadline_even_without_input_events() {
        let start = Instant::now();
        let mut watchdog = ProgressWatchdog::new(12, start);

        assert!(!watchdog.stalled(13, start + Duration::from_secs(4)));
        assert!(!watchdog.stalled(13, start + Duration::from_secs(8)));
        assert!(watchdog.stalled(13, start + Duration::from_secs(9)));
    }
}
