//! Always-on, bounded local flight recorder for production input metrics.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};

const DEFAULT_REPORT_INTERVAL_S: u64 = 5;
const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024;
const DEFAULT_MAX_FILES: usize = 8;
const QUEUE_CAPACITY: usize = 4_096;

static SENDER: OnceLock<SyncSender<String>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct RecorderConfig {
    pub path: PathBuf,
    pub max_bytes: u64,
    /// Maximum number of files including the active log.
    pub max_files: usize,
}

impl RecorderConfig {
    #[must_use]
    pub fn from_env() -> Self {
        let path = std::env::var_os("PADJUTSU_METRICS_LOG")
            .map(PathBuf::from)
            .unwrap_or_else(default_log_path);
        let max_bytes = env_u64("PADJUTSU_METRICS_MAX_BYTES")
            .unwrap_or(DEFAULT_MAX_BYTES)
            .max(1_024);
        let max_files = env_u64("PADJUTSU_METRICS_MAX_FILES")
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_MAX_FILES)
            .clamp(1, 64);
        Self {
            path,
            max_bytes,
            max_files,
        }
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

#[must_use]
pub fn default_log_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Library/Logs/padjutsu/metrics.jsonl")
}

#[must_use]
pub fn enabled() -> bool {
    std::env::var("PADJUTSU_METRICS")
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

#[must_use]
pub fn report_interval() -> Duration {
    let seconds = env_u64("PADJUTSU_METRICS_INTERVAL_S")
        .unwrap_or(DEFAULT_REPORT_INTERVAL_S)
        .clamp(5, 3_600);
    Duration::from_secs(seconds)
}

/// Start the single background writer. Metric producers never wait for disk.
pub fn init(config: RecorderConfig) -> io::Result<PathBuf> {
    if SENDER.get().is_some() {
        return Ok(config.path);
    }
    if let Some(parent) = config.path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Validate the destination synchronously so startup reports bad paths.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.path)?;

    let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
    if SENDER.set(sender).is_err() {
        return Ok(config.path);
    }
    let path = config.path.clone();
    std::thread::Builder::new()
        .name("metrics-recorder".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            if let Err(error) = writer_loop(&config, &receiver) {
                let mut stderr = io::stderr().lock();
                let _ = writeln!(
                    stderr,
                    "[metrics-recorder] stopped writing {}: {error}",
                    config.path.display()
                );
            }
        })?;
    Ok(path)
}

pub fn init_default() -> io::Result<PathBuf> {
    init(RecorderConfig::from_env())
}

pub fn append_marker(message: &str) -> io::Result<PathBuf> {
    let config = RecorderConfig::from_env();
    if let Some(parent) = config.path.parent() {
        fs::create_dir_all(parent)?;
    }
    let unix_ms = unix_ms_now();
    let local: DateTime<Local> = SystemTime::now().into();
    let line = render_record(
        unix_ms,
        &local.to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
        "marker",
        message,
        0,
    );
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.path)?;
    writeln!(file, "{line}")?;
    Ok(config.path)
}

pub fn read_window(
    path: &Path,
    from_unix_ms: u128,
    to_unix_ms: u128,
) -> io::Result<Vec<String>> {
    let rotated_files = RecorderConfig::from_env().max_files.saturating_sub(1);
    let mut lines = Vec::new();
    for index in (1..=rotated_files).rev() {
        read_matching_lines(
            &rotated_path(path, index),
            from_unix_ms,
            to_unix_ms,
            &mut lines,
        )?;
    }
    read_matching_lines(path, from_unix_ms, to_unix_ms, &mut lines)?;
    Ok(lines)
}

fn read_matching_lines(
    path: &Path,
    from_unix_ms: u128,
    to_unix_ms: u128,
    output: &mut Vec<String>,
) -> io::Result<()> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    output.extend(content.lines().filter_map(|line| {
        let unix_ms = extract_unix_ms(line)?;
        (unix_ms >= from_unix_ms && unix_ms <= to_unix_ms).then(|| line.to_owned())
    }));
    Ok(())
}

fn extract_unix_ms(line: &str) -> Option<u128> {
    let rest = line.split_once("\"unix_ms\":")?.1;
    let digits = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().ok()
}

#[must_use]
pub fn classify_incident(line: &str) -> Option<&'static str> {
    if line.contains("\"component\":\"marker\"") {
        return Some("user-marker");
    }
    if line.contains("[service-metrics] event=start") {
        return Some("service-restart");
    }
    if value_after(line, "\"dropped_before\":") > 0 {
        return Some("metrics-recorder-overflow");
    }
    if value_after(line, "display_reconfiguration_events=") > 0 {
        return Some("display-reconfiguration");
    }
    if value_after(line, "subscriber_drops=") > 0
        || value_after(line, " dropped=") > 0
    {
        return Some("input-or-performer-queue-drop");
    }
    if value_after(line, "mouse_warp_over_16ms=") > 0
        || value_after(line, "mouse_warp_over_50ms=") > 0
    {
        return Some("windowserver-cursor-warp-stall");
    }
    if value_after(line, "mouse_event_post_over_16ms=") > 0
        || value_after(line, "mouse_event_post_over_50ms=") > 0
        || value_after(line, "mouse_post_over_16ms=") > 0
        || value_after(line, "mouse_post_over_50ms=") > 0
    {
        return Some("windowserver-event-post-stall");
    }
    if value_after(line, "queue_wait_over_4ms=") > 0 {
        return Some("performer-queue-stall");
    }
    if (line.contains("[wake-metrics]") && value_after(line, "over_4ms=") > 0)
        || value_after(line, "missed_periods=") > 0
        || value_after(line, "gap_over_2x=") > 0
    {
        return Some("scheduler-starvation");
    }
    if value_after(line, "cursor_recovery_warps=") > 0 {
        return Some("cursor-recovered-after-stall");
    }
    if value_after(line, "cursor_stalled=") >= 3
        && max_in_summary(line, "cursor_tracking_error_axis_px(") >= 8
    {
        return Some("cursor-not-applied");
    }
    if line.contains("sdl_loop_gap:") && value_after(line, "max=") > 8_000 {
        return Some("gamepad-loop-stall");
    }
    None
}

fn value_after(line: &str, key: &str) -> u64 {
    let Some(rest) = line.split_once(key).map(|(_, rest)| rest) else {
        return 0;
    };
    rest.chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

fn max_in_summary(line: &str, key: &str) -> u64 {
    let Some(summary) = line
        .split_once(key)
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(summary, _)| summary)
    else {
        return 0;
    };
    value_after(summary, "max=")
}

/// Enqueue a timestamped metric for durable storage.
/// A full queue drops the record rather than delaying an input thread.
pub fn record(component: &str, arguments: fmt::Arguments<'_>) {
    if !enabled() {
        return;
    }
    let message = arguments.to_string();
    let unix_ms = unix_ms_now();
    let local: DateTime<Local> = SystemTime::now().into();
    let dropped_before = DROPPED.swap(0, Ordering::Relaxed);
    let line = render_record(
        unix_ms,
        &local.to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
        component,
        &message,
        dropped_before,
    );
    let Some(sender) = SENDER.get() else {
        return;
    };
    match sender.try_send(line) {
        Ok(()) => {}
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
            let newly_dropped = dropped_before.saturating_add(1);
            let _ = DROPPED.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |current| Some(current.saturating_add(newly_dropped)),
            );
        }
    }
}

#[macro_export]
macro_rules! metric {
    ($component:expr, $($arg:tt)*) => {
        $crate::record($component, format_args!($($arg)*))
    };
}

fn unix_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn render_record(
    unix_ms: u128,
    timestamp: &str,
    component: &str,
    message: &str,
    dropped_before: u64,
) -> String {
    format!(
        "{{\"ts\":\"{}\",\"unix_ms\":{},\"component\":\"{}\",\"dropped_before\":{},\"message\":\"{}\"}}",
        escape_json(timestamp),
        unix_ms,
        escape_json(component),
        dropped_before,
        escape_json(message),
    )
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                use fmt::Write as _;
                let _ = write!(escaped, "\\u{:04x}", u32::from(character));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn writer_loop(
    config: &RecorderConfig,
    receiver: &mpsc::Receiver<String>,
) -> io::Result<()> {
    let (mut writer, mut bytes_written) = open_writer(&config.path)?;
    loop {
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(line) => {
                let record_bytes = line.len() as u64 + 1;
                if bytes_written.saturating_add(record_bytes) > config.max_bytes {
                    writer.flush()?;
                    drop(writer);
                    rotate(&config.path, config.max_files)?;
                    (writer, bytes_written) = open_writer(&config.path)?;
                }
                writeln!(writer, "{line}")?;
                bytes_written = bytes_written.saturating_add(record_bytes);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                writer.flush()?;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return writer.flush();
            }
        }
    }
}

fn open_writer(path: &Path) -> io::Result<(BufWriter<File>, u64)> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let len = file.metadata()?.len();
    Ok((BufWriter::new(file), len))
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(format!(".{index}"));
    PathBuf::from(value)
}

fn rotate(path: &Path, max_files: usize) -> io::Result<()> {
    let rotated_files = max_files.saturating_sub(1);
    if rotated_files == 0 {
        if path.exists() {
            fs::remove_file(path)?;
        }
        return Ok(());
    }
    let oldest = rotated_path(path, rotated_files);
    if oldest.exists() {
        fs::remove_file(oldest)?;
    }
    for index in (1..rotated_files).rev() {
        let source = rotated_path(path, index);
        if source.exists() {
            fs::rename(source, rotated_path(path, index + 1))?;
        }
    }
    if path.exists() {
        fs::rename(path, rotated_path(path, 1))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_is_valid_single_line_json_shape_with_timestamp() {
        let line = render_record(
            1_723_456_789_123,
            "2026-08-12T16:12:03.123+05:00",
            "performer",
            "[performer-metrics] max=\"9\"\nnext",
            2,
        );
        assert!(line.starts_with("{\"ts\":\"2026-08-12T16:12:03.123+05:00\""));
        assert!(line.contains("\"unix_ms\":1723456789123"));
        assert!(line.contains("\"dropped_before\":2"));
        assert!(line.contains("max=\\\"9\\\"\\nnext"));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn rotation_keeps_a_bounded_newest_history() {
        let root = std::env::temp_dir().join(format!(
            "padjutsu-metrics-{}-{}",
            std::process::id(),
            unix_ms_now()
        ));
        fs::create_dir_all(&root).expect("create test directory");
        let path = root.join("metrics.jsonl");
        fs::write(&path, "current").expect("write current");
        fs::write(rotated_path(&path, 1), "previous").expect("write previous");

        rotate(&path, 3).expect("rotate");

        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(rotated_path(&path, 1)).expect("read newest"),
            "current"
        );
        assert_eq!(
            fs::read_to_string(rotated_path(&path, 2)).expect("read oldest"),
            "previous"
        );
        assert!(!rotated_path(&path, 3).exists());
        fs::remove_dir_all(root).expect("cleanup test directory");
    }

    #[test]
    fn incident_classifier_explains_common_failure_layers() {
        assert_eq!(
            classify_incident("{\"component\":\"marker\",\"message\":\"lag\"}"),
            Some("user-marker")
        );
        assert_eq!(
            classify_incident("{\"dropped_before\":3}"),
            Some("metrics-recorder-overflow")
        );
        assert_eq!(
            classify_incident("mouse_warp_over_16ms=2 mouse_warp_over_50ms=0"),
            Some("windowserver-cursor-warp-stall")
        );
        assert_eq!(
            classify_incident(
                "mouse_event_post_over_16ms=2 mouse_event_post_over_50ms=0"
            ),
            Some("windowserver-event-post-stall")
        );
        assert_eq!(
            classify_incident("mouse_post_over_16ms=2 mouse_post_over_50ms=0"),
            Some("windowserver-event-post-stall")
        );
        assert_eq!(
            classify_incident("[wake-metrics] over_4ms=3 missed_periods=0"),
            Some("scheduler-starvation")
        );
        assert_eq!(
            classify_incident(
                "cursor_tracking_error_axis_px(n=20,avg=2,max=12) cursor_stalled=4"
            ),
            Some("cursor-not-applied")
        );
        assert_eq!(
            classify_incident(
                "cursor_stalled=4 cursor_recovery_warps=1 cursor_stall_sequence_max=2"
            ),
            Some("cursor-recovered-after-stall")
        );
        assert_eq!(
            classify_incident(
                "cursor_tracking_error_axis_px(n=20,avg=0,max=4) cursor_stalled=1"
            ),
            None
        );
        assert_eq!(
            classify_incident("queue_wait_over_4ms=1 queue_wait_over_16ms=0"),
            Some("performer-queue-stall")
        );
    }

    #[test]
    fn window_reader_merges_rotated_history_oldest_first() {
        let root = std::env::temp_dir().join(format!(
            "padjutsu-metrics-read-{}-{}",
            std::process::id(),
            unix_ms_now()
        ));
        fs::create_dir_all(&root).expect("create test directory");
        let path = root.join("metrics.jsonl");
        fs::write(
            rotated_path(&path, 1),
            "{\"unix_ms\":100,\"message\":\"old\"}\n",
        )
        .expect("write rotated");
        fs::write(&path, "{\"unix_ms\":200,\"message\":\"new\"}\n")
            .expect("write current");

        let lines = read_window(&path, 50, 250).expect("read window");

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("old"));
        assert!(lines[1].contains("new"));
        fs::remove_dir_all(root).expect("cleanup test directory");
    }
}
