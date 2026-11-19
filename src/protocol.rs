//! serve: requests as JSON lines on stdin, a reply line to each on stdout in
//! the order they came, with the bytes of a frame after its reply.
//! protocol.json is the schema of both, which the app holds its own to.

use std::io::{BufRead, Write};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

use crate::{caps, frame};

pub const SCHEMA: &str = include_str!("../protocol.json");

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
enum Request {
    /// What this build decodes, encodes, reads and writes.
    Caps { id: u64 },
    /// A frame of a video, as a PNG: from `at` of the way through it (0 to
    /// 1), deinterlaced, its pixels made square, and no larger than `size`
    /// either way.
    Frame {
        id: u64,
        path: String,
        at: f64,
        size: u32,
    },
    /// Ends the program once what came before it is answered.
    Stop,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FrameReply {
    width: u32,
    height: u32,
    /// How many bytes of PNG follow the line.
    bytes: usize,
}

/// Why a request couldn't be done: a code the caller goes by, and words for
/// its log.
#[derive(Serialize)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
}

impl Failure {
    pub fn new(code: &'static str, message: impl ToString) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

fn line(out: &mut impl Write, value: &impl Serialize) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")
}

pub fn serve() -> ExitCode {
    let stdin = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    for request in stdin.lines() {
        let Ok(request) = request else { break };
        if request.trim().is_empty() {
            continue;
        }
        let written = match serde_json::from_str::<Request>(&request) {
            Err(error) => line(
                &mut out,
                &serde_json::json!({ "error": Failure::new("request", error) }),
            ),
            Ok(Request::Stop) => break,
            Ok(Request::Caps { id }) => line(
                &mut out,
                &serde_json::json!({ "id": id, "ok": caps::caps() }),
            ),
            Ok(Request::Frame { id, path, at, size }) => match frame::frame(&path, at, size) {
                Ok(png) => line(
                    &mut out,
                    &serde_json::json!({ "id": id, "ok": FrameReply {
						width: png.width,
						height: png.height,
						bytes: png.data.len(),
					} }),
                )
                .and_then(|()| out.write_all(&png.data)),
                Err(failure) => line(&mut out, &serde_json::json!({ "id": id, "error": failure })),
            },
        };
        if written.and_then(|()| out.flush()).is_err() {
            // The caller is gone.
            break;
        }
    }
    ExitCode::SUCCESS
}
