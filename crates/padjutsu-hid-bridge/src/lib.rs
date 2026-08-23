use std::io::{self, Read, Write};

pub const LOCAL_REPORT_LEN: usize = 5;
pub const CLIENT_PROTOCOL_VERSION: u16 = 7;
const MAX_MESSAGE_PAYLOAD_LEN: usize = 1024;
const REQUEST_ID_LEN: usize = size_of::<u64>();
const MAX_BODY_LEN: usize = 1 + REQUEST_ID_LEN + MAX_MESSAGE_PAYLOAD_LEN;
const MESSAGE_TYPE_REQUEST: u8 = 4;
const MESSAGE_TYPE_RESPONSE: u8 = 5;
const MESSAGE_TYPE_HEARTBEAT: u8 = 0;
const MESSAGE_TYPE_HEALTH_CHECK: u8 = 2;
const MESSAGE_TYPE_HEALTH_CHECK_RESPONSE: u8 = 3;
const REQUEST_POINTING_INITIALIZE: u8 = 3;
const REQUEST_POST_POINTING_REPORT: u8 = 11;
const RESPONSE_POINTING_READY: u8 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalMouseReport(pub [u8; LOCAL_REPORT_LEN]);

impl LocalMouseReport {
    pub fn buttons(self) -> u8 {
        self.0[0]
    }
}

pub fn coalesce_same_button_runs(
    reports: impl IntoIterator<Item = LocalMouseReport>,
) -> Vec<LocalMouseReport> {
    let mut result: Vec<LocalMouseReport> = Vec::new();
    for report in reports {
        if result
            .last()
            .is_some_and(|previous| previous.buttons() == report.buttons())
        {
            *result.last_mut().expect("last element checked above") = report;
        } else {
            result.push(report);
        }
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub request_id: Option<u64>,
    pub payload: Vec<u8>,
}

pub fn initialize_payload() -> Vec<u8> {
    let mut payload = Vec::with_capacity(3);
    payload.extend_from_slice(&CLIENT_PROTOCOL_VERSION.to_le_bytes());
    payload.push(REQUEST_POINTING_INITIALIZE);
    payload
}

pub fn pointing_report_payload(report: LocalMouseReport) -> Vec<u8> {
    let [buttons, x, y, wheel, pan] = report.0;
    let mut payload = Vec::with_capacity(12);
    payload.extend_from_slice(&CLIENT_PROTOCOL_VERSION.to_le_bytes());
    payload.push(REQUEST_POST_POINTING_REPORT);
    payload.extend_from_slice(&u32::from(buttons).to_le_bytes());
    payload.extend_from_slice(&[x, y, wheel, pan]);
    payload
}

pub fn request_frame(request_id: u64, payload: &[u8]) -> Vec<u8> {
    request_response_frame(MESSAGE_TYPE_REQUEST, request_id, payload)
}

pub fn response_frame(request_id: u64, payload: &[u8]) -> Vec<u8> {
    request_response_frame(MESSAGE_TYPE_RESPONSE, request_id, payload)
}

pub fn heartbeat_frame() -> Vec<u8> {
    simple_frame(MESSAGE_TYPE_HEARTBEAT, &[])
}

fn request_response_frame(kind: u8, request_id: u64, payload: &[u8]) -> Vec<u8> {
    let body_len = 1 + REQUEST_ID_LEN + payload.len();
    let mut frame = Vec::with_capacity(4 + body_len);
    let wire_body_len = u32::try_from(body_len).expect("frame body exceeds u32");
    frame.extend_from_slice(&wire_body_len.to_be_bytes());
    frame.push(kind);
    frame.extend_from_slice(&request_id.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn simple_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let body_len = 1 + payload.len();
    let mut frame = Vec::with_capacity(4 + body_len);
    let wire_body_len = u32::try_from(body_len).expect("frame body exceeds u32");
    frame.extend_from_slice(&wire_body_len.to_be_bytes());
    frame.push(kind);
    frame.extend_from_slice(payload);
    frame
}

pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<Frame> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header)?;
    let body_len = u32::from_be_bytes(header) as usize;
    if body_len == 0 || body_len > MAX_BODY_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Karabiner frame body length: {body_len}"),
        ));
    }

    let mut body = vec![0_u8; body_len];
    reader.read_exact(&mut body)?;
    let kind = body[0];
    if kind == MESSAGE_TYPE_REQUEST || kind == MESSAGE_TYPE_RESPONSE {
        if body.len() < 1 + REQUEST_ID_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Karabiner request/response frame has no request id",
            ));
        }
        let request_id = u64::from_be_bytes(
            body[1..=REQUEST_ID_LEN]
                .try_into()
                .expect("slice length checked above"),
        );
        Ok(Frame {
            kind,
            request_id: Some(request_id),
            payload: body[1 + REQUEST_ID_LEN..].to_vec(),
        })
    } else {
        Ok(Frame {
            kind,
            request_id: None,
            payload: body[1..].to_vec(),
        })
    }
}

pub fn write_all<W: Write>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(bytes)?;
    writer.flush()
}

pub struct KarabinerConnection<S> {
    stream: S,
    next_request_id: u64,
    pointing_ready: bool,
}

impl<S> KarabinerConnection<S>
where
    S: Read + Write,
{
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            next_request_id: 1,
            pointing_ready: false,
        }
    }

    pub fn initialize_pointing(&mut self) -> io::Result<()> {
        let response = self.send_request(&initialize_payload())?;
        self.observe_status(&response);
        Ok(())
    }

    pub fn wait_until_pointing_ready(&mut self) -> io::Result<()> {
        while !self.pointing_ready {
            self.pump_once()?;
        }
        Ok(())
    }

    pub fn post_report(&mut self, report: LocalMouseReport) -> io::Result<()> {
        self.send_request(&pointing_report_payload(report))?;
        Ok(())
    }

    pub fn send_heartbeat(&mut self) -> io::Result<()> {
        write_all(&mut self.stream, &heartbeat_frame())
    }

    pub fn pointing_ready(&self) -> bool {
        self.pointing_ready
    }

    pub fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    pub fn pump_once(&mut self) -> io::Result<()> {
        let frame = read_frame(&mut self.stream)?;
        self.handle_unsolicited(&frame)
    }

    fn send_request(&mut self, payload: &[u8]) -> io::Result<Vec<u8>> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        write_all(&mut self.stream, &request_frame(request_id, payload))?;

        loop {
            let frame = read_frame(&mut self.stream)?;
            if frame.kind == MESSAGE_TYPE_RESPONSE
                && frame.request_id == Some(request_id)
            {
                self.observe_status(&frame.payload);
                return Ok(frame.payload);
            }
            self.handle_unsolicited(&frame)?;
        }
    }

    fn handle_unsolicited(&mut self, frame: &Frame) -> io::Result<()> {
        match frame.kind {
            MESSAGE_TYPE_HEARTBEAT | MESSAGE_TYPE_HEALTH_CHECK_RESPONSE => Ok(()),
            MESSAGE_TYPE_HEALTH_CHECK => write_all(
                &mut self.stream,
                &simple_frame(MESSAGE_TYPE_HEALTH_CHECK_RESPONSE, &[]),
            ),
            MESSAGE_TYPE_REQUEST => {
                self.observe_status(&frame.payload);
                let request_id = frame.request_id.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Karabiner server request has no request id",
                    )
                })?;
                write_all(&mut self.stream, &response_frame(request_id, &[]))
            }
            MESSAGE_TYPE_RESPONSE => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected Karabiner response id {:?}", frame.request_id),
            )),
            kind => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported Karabiner frame type: {kind}"),
            )),
        }
    }

    fn observe_status(&mut self, payload: &[u8]) {
        for pair in payload.chunks_exact(2) {
            if pair[0] == RESPONSE_POINTING_READY {
                self.pointing_ready = pair[1] != 0;
            }
        }
    }

    #[cfg(test)]
    fn into_stream(self) -> S {
        self.stream
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Write};

    use super::*;

    struct ScriptedStream {
        reads: Cursor<Vec<u8>>,
        writes: Vec<u8>,
    }

    impl ScriptedStream {
        fn new(reads: Vec<u8>) -> Self {
            Self {
                reads: Cursor::new(reads),
                writes: Vec::new(),
            }
        }
    }

    impl Read for ScriptedStream {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads.read(buffer)
        }
    }

    impl Write for ScriptedStream {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn initialize_request_contains_protocol_version_and_request_kind() {
        assert_eq!(initialize_payload(), vec![7, 0, 3]);
    }

    #[test]
    fn local_report_expands_buttons_to_karabiners_packed_u32() {
        let payload =
            pointing_report_payload(LocalMouseReport([0b1_0101, 129, 126, 253, 4]));
        assert_eq!(
            payload,
            vec![
                7, 0, 11, // protocol v7, post_pointing_input_report
                0b1_0101, 0, 0, 0, // packed little-endian 32-bit buttons
                129, 126, 253, 4, // signed x/y/wheel/pan bytes
            ]
        );
    }

    #[test]
    fn request_frame_uses_big_endian_length_and_request_id() {
        assert_eq!(
            request_frame(0x0102_0304_0506_0708, &[7, 0, 3]),
            vec![
                0, 0, 0, 12, // body length: type + id + payload
                4,  // request message type
                1, 2, 3, 4, 5, 6, 7, 8, // request id
                7, 0, 3,
            ]
        );
    }

    #[test]
    fn heartbeat_is_an_empty_type_zero_frame() {
        assert_eq!(heartbeat_frame(), vec![0, 0, 0, 1, 0]);
    }

    #[test]
    fn response_frame_round_trips_through_parser() {
        let bytes = response_frame(42, &[5, 1]);
        let frame = read_frame(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(
            frame,
            Frame {
                kind: 5,
                request_id: Some(42),
                payload: vec![5, 1],
            }
        );
    }

    #[test]
    fn parser_rejects_oversized_frames_before_allocating_body() {
        let mut bytes = Cursor::new(vec![0, 0, 8, 0]);
        assert_eq!(
            read_frame(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn connection_answers_health_checks_and_server_status_requests() {
        let mut reads = simple_frame(MESSAGE_TYPE_HEALTH_CHECK, &[]);
        reads.extend(request_frame(99, &[RESPONSE_POINTING_READY, 1]));
        reads.extend(response_frame(1, &[]));
        let mut connection = KarabinerConnection::new(ScriptedStream::new(reads));

        connection.initialize_pointing().unwrap();

        assert!(connection.pointing_ready());
        let writes = connection.into_stream().writes;
        let mut expected = request_frame(1, &initialize_payload());
        expected.extend(simple_frame(MESSAGE_TYPE_HEALTH_CHECK_RESPONSE, &[]));
        expected.extend(response_frame(99, &[]));
        assert_eq!(writes, expected);
    }

    #[test]
    fn post_report_waits_for_its_matching_response() {
        let stream = ScriptedStream::new(response_frame(1, &[]));
        let mut connection = KarabinerConnection::new(stream);
        let report = LocalMouseReport([1, 2, 3, 4, 5]);

        connection.post_report(report).unwrap();

        assert_eq!(
            connection.into_stream().writes,
            request_frame(1, &pointing_report_payload(report))
        );
    }

    #[test]
    fn coalescing_drops_motion_debt_but_preserves_button_transitions() {
        let reports = [
            LocalMouseReport([0, 1, 0, 0, 0]),
            LocalMouseReport([0, 2, 0, 0, 0]),
            LocalMouseReport([1, 0, 0, 0, 0]),
            LocalMouseReport([1, 3, 0, 0, 0]),
            LocalMouseReport([0, 0, 0, 0, 0]),
        ];
        assert_eq!(
            coalesce_same_button_runs(reports),
            vec![reports[1], reports[3], reports[4]]
        );
    }
}
