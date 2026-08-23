#![allow(clippy::print_stderr)]

use std::env;
use std::ffi::CString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use padjutsu_hid_bridge::{KarabinerConnection, LocalMouseReport, LOCAL_REPORT_LEN};

const DEFAULT_SOCKET_PATH: &str = "/var/run/padjutsu-hid-bridge.sock";
const DEFAULT_KARABINER_SOCKET_PATH: &str = "/Library/Application Support/org.pqrs/tmp/rootonly/karabiner_virtual_hid_device_service.sock";
const BRIDGE_OK: u8 = 0;
const BRIDGE_ERROR: u8 = 1;
const REPORT_QUEUE_CAPACITY: usize = 1024;
const MAX_PEEK_FRAME_LEN: usize = 2048;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);

struct Options {
    socket_path: PathBuf,
    karabiner_socket_path: PathBuf,
    owner: Option<(u32, u32)>,
}

fn main() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("padjutsu-hid-bridge must run as root");
        std::process::exit(77);
    }
    let options = match parse_options(env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(64);
        }
    };
    if let Err(error) = serve(&options) {
        eprintln!("padjutsu-hid-bridge failed: {error}");
        std::process::exit(1);
    }
}

fn parse_options(
    arguments: impl Iterator<Item = String>,
) -> Result<Options, String> {
    let mut socket_path = PathBuf::from(DEFAULT_SOCKET_PATH);
    let mut karabiner_socket_path = PathBuf::from(DEFAULT_KARABINER_SOCKET_PATH);
    let mut owner_uid = None;
    let mut owner_gid = None;
    let mut arguments = arguments;
    while let Some(argument) = arguments.next() {
        let value = match argument.as_str() {
            "--socket" | "--karabiner-socket" | "--owner-uid" | "--owner-gid" => {
                arguments
                    .next()
                    .ok_or_else(|| format!("missing value for {argument}"))?
            }
            _ => return Err(format!("unknown argument: {argument}")),
        };
        match argument.as_str() {
            "--socket" => socket_path = PathBuf::from(value),
            "--karabiner-socket" => karabiner_socket_path = PathBuf::from(value),
            "--owner-uid" => {
                owner_uid = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid owner uid: {value}"))?,
                );
            }
            "--owner-gid" => {
                owner_gid = Some(
                    value
                        .parse()
                        .map_err(|_| format!("invalid owner gid: {value}"))?,
                );
            }
            _ => unreachable!("matched above"),
        }
    }
    let owner = match (owner_uid, owner_gid) {
        (Some(uid), Some(gid)) => Some((uid, gid)),
        (None, None) => None,
        _ => {
            return Err(
                "--owner-uid and --owner-gid must be provided together".into()
            )
        }
    };
    Ok(Options {
        socket_path,
        karabiner_socket_path,
        owner,
    })
}

fn serve(options: &Options) -> io::Result<()> {
    set_user_interactive_qos("bridge-reader");
    let (owner_uid, owner_gid) = options.owner.map_or_else(console_owner, Ok)?;
    let listener = prepare_listener(&options.socket_path, owner_uid, owner_gid)?;
    eprintln!(
        "padjutsu-hid-bridge listening at {} for uid={} gid={}",
        options.socket_path.display(),
        owner_uid,
        owner_gid
    );
    for connection in listener.incoming() {
        match connection {
            Ok(mut client) => {
                if let Err(error) =
                    handle_client(&mut client, &options.karabiner_socket_path)
                {
                    let _ = client.write_all(&[BRIDGE_ERROR]);
                    eprintln!("padjutsu-hid-bridge client session failed: {error}");
                }
            }
            Err(error) => eprintln!("padjutsu-hid-bridge accept failed: {error}"),
        }
    }
    Ok(())
}

fn console_owner() -> io::Result<(u32, u32)> {
    let metadata = fs::metadata("/dev/console")?;
    Ok((metadata.uid(), metadata.gid()))
}

fn prepare_listener(
    path: &Path,
    owner_uid: u32,
    owner_gid: u32,
) -> io::Result<UnixListener> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("refusing to replace non-socket path: {}", path.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let c_path =
        CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "socket path contains NUL")
        })?;
    let result = unsafe { libc::chown(c_path.as_ptr(), owner_uid, owner_gid) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(listener)
}

fn handle_client(client: &mut UnixStream, karabiner_path: &Path) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_millis(100)))?;
    client.set_write_timeout(Some(Duration::from_millis(100)))?;

    let karabiner_stream = UnixStream::connect(karabiner_path)?;
    karabiner_stream.set_write_timeout(Some(Duration::from_millis(100)))?;
    let mut karabiner = KarabinerConnection::new(karabiner_stream);
    karabiner.initialize_pointing()?;
    karabiner.wait_until_pointing_ready()?;
    let (reports_tx, reports_rx) = mpsc::sync_channel(REPORT_QUEUE_CAPACITY);
    let delivery = thread::Builder::new()
        .name("karabiner-hid-delivery".into())
        .spawn(move || run_delivery(karabiner, reports_rx))?;
    client.write_all(&[BRIDGE_OK])?;

    let mut report = [0_u8; LOCAL_REPORT_LEN];
    let mut filled = 0;
    loop {
        match client.read(&mut report[filled..]) {
            Ok(0) => break,
            Ok(read) => {
                filled += read;
                if filled == report.len() {
                    match reports_tx.try_send(LocalMouseReport(report)) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            return Err(io::Error::new(
                                io::ErrorKind::WouldBlock,
                                "virtual HID delivery queue is full",
                            ));
                        }
                        Err(TrySendError::Disconnected(_)) => {
                            break;
                        }
                    }
                    filled = 0;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if delivery.is_finished() {
                    break;
                }
            }
            Err(error) => return Err(error),
        }
    }
    drop(reports_tx);
    delivery
        .join()
        .map_err(|_| io::Error::other("Karabiner delivery thread panicked"))?
}

#[allow(clippy::needless_pass_by_value)]
fn run_delivery(
    mut karabiner: KarabinerConnection<UnixStream>,
    reports: Receiver<LocalMouseReport>,
) -> io::Result<()> {
    set_user_interactive_qos("karabiner-hid-delivery");
    let mut pending = None;
    let mut last_heartbeat = Instant::now();
    loop {
        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            karabiner
                .send_heartbeat()
                .map_err(|error| with_context(error, "send Karabiner heartbeat"))?;
            last_heartbeat = Instant::now();
        }
        let report = match pending.take() {
            Some(report) => report,
            None => match reports.recv_timeout(Duration::from_millis(100)) {
                Ok(report) => report,
                // Karabiner health checks remain buffered while idle. `post_report`
                // drains and answers them before waiting for its matching response.
                // Darwin rejects sub-second SO_RCVTIMEO values on this socket, so
                // changing the timeout here would tear down an otherwise healthy
                // virtual-HID session.
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if karabiner_frame_ready(karabiner.stream_mut())? {
                        karabiner.pump_once().map_err(|error| {
                            with_context(error, "pump idle Karabiner frame")
                        })?;
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            },
        };
        let mut latest = report;
        loop {
            match reports.try_recv() {
                Ok(next) if next.buttons() == latest.buttons() => latest = next,
                Ok(next) => {
                    pending = Some(next);
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        karabiner.post_report(latest).map_err(|error| {
            with_context(error, "post Karabiner pointing report")
        })?;
    }
}

fn karabiner_frame_ready(stream: &UnixStream) -> io::Result<bool> {
    let mut bytes = [0_u8; MAX_PEEK_FRAME_LEN];
    let read = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            libc::MSG_DONTWAIT | libc::MSG_PEEK,
        )
    };
    if read < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::WouldBlock {
            Ok(false)
        } else {
            Err(error)
        };
    }
    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "Karabiner service closed the connection",
        ));
    }
    let read = usize::try_from(read).expect("negative recv result handled above");
    Ok(complete_wire_frame(&bytes[..read]))
}

fn complete_wire_frame(bytes: &[u8]) -> bool {
    let Some(header) = bytes.get(..4) else {
        return false;
    };
    let body_len = u32::from_be_bytes(
        header
            .try_into()
            .expect("four-byte frame header checked above"),
    ) as usize;
    body_len > 0 && bytes.len() >= 4 + body_len
}

fn set_user_interactive_qos(name: &str) {
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
    eprintln!(
        "padjutsu-hid-bridge thread policy name={name} requested=user_interactive result={}",
        if result == 0 { "success" } else { "failure" }
    );
}

#[allow(clippy::needless_pass_by_value)]
fn with_context(error: io::Error, context: &str) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::complete_wire_frame;

    #[test]
    fn idle_pump_waits_for_a_complete_wire_frame() {
        assert!(!complete_wire_frame(&[]));
        assert!(!complete_wire_frame(&[0, 0, 0, 2, 7]));
        assert!(complete_wire_frame(&[0, 0, 0, 2, 7, 8]));
    }
}
