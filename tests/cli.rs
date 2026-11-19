//! The program as its caller has it: serve's requests and replies, and what
//! stream writes, on the clips in tests/fixtures. build.sh --test says which
//! groups of codecs.conf it was built with (FFMPEG_RS_GROUPS), as what the
//! program can do depends on them.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const PROGRAM: &str = env!("CARGO_BIN_EXE_ffmpeg-rs");

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn built_with(group: &str) -> bool {
    std::env::var("FFMPEG_RS_GROUPS")
        .unwrap_or_else(|_| "free".into())
        .split(',')
        .any(|built| built == group)
}

// Sends serve the requests, then stop, and gives its replies, each with the
// bytes that came after it.
fn serve(requests: &[Value]) -> Vec<(Value, Vec<u8>)> {
    let mut child = Command::new(PROGRAM)
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the program starts");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for request in requests {
            writeln!(stdin, "{request}").expect("a request is written");
        }
        writeln!(stdin, r#"{{"op":"stop"}}"#).expect("stop is written");
    }
    let output = child.wait_with_output().expect("the program ends");
    assert!(
        output.status.success(),
        "serve ended with {}",
        output.status
    );
    let mut rest = output.stdout.as_slice();
    let mut replies = Vec::new();
    while !rest.is_empty() {
        let end = rest.iter().position(|&byte| byte == b'\n').expect("a line");
        let reply: Value = serde_json::from_slice(&rest[..end]).expect("JSON");
        rest = &rest[end + 1..];
        let bytes = reply["ok"]["bytes"].as_u64().unwrap_or(0) as usize;
        replies.push((reply, rest[..bytes].to_vec()));
        rest = &rest[bytes..];
    }
    replies
}

fn stream(args: &[&str]) -> Output {
    Command::new(PROGRAM)
        .arg("stream")
        .args(args)
        .output()
        .expect("the program runs")
}

fn has(bytes: &[u8], text: &str) -> bool {
    bytes
        .windows(text.len())
        .any(|window| window == text.as_bytes())
}

#[test]
fn it_is_lgpl_alone() {
    let output = Command::new(PROGRAM).arg("--version").output().unwrap();
    let version: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(version["license"], "LGPL version 2.1 or later");
    let configuration = version["configuration"].as_str().unwrap();
    assert!(!configuration.contains("--enable-gpl"));
    assert!(!configuration.contains("--enable-nonfree"));
}

#[test]
fn caps_say_what_it_was_built_with() {
    let replies = serve(&[serde_json::json!({ "op": "caps", "id": 1 })]);
    let caps = &replies[0].0["ok"];
    let listed = |kind: &str, name: &str| {
        caps[kind]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == name)
    };
    assert_eq!(replies[0].0["id"], 1);
    for decoder in ["dvvideo", "mpeg2video", "mpeg4", "wmv2", "ac3", "wmav2"] {
        assert!(listed("decoders", decoder), "{decoder}");
    }
    assert!(listed("encoders", "png") && listed("encoders", "libvpx-vp9"));
    assert!(listed("muxers", "mp4"));
    // A group under patent is in it only when it was built with it.
    assert_eq!(listed("decoders", "h264"), built_with("h264"));
    assert_eq!(listed("decoders", "hevc"), built_with("hevc"));
    assert_eq!(listed("decoders", "vc1"), built_with("vc1"));
    assert_eq!(listed("encoders", "aac"), built_with("aac-encoder"));
}

#[test]
fn a_frame_is_square_fitted_and_deinterlaced() {
    let frame = |id: u64, name: &str| serde_json::json!({ "op": "frame", "id": id, "path": fixture(name), "at": 0.5, "size": 512 });
    let replies = serve(&[
        frame(1, "mpeg4.avi"),
        frame(2, "interlaced.vob"),
        frame(3, "h264.mts"),
        serde_json::json!({ "op": "frame", "id": 4, "path": fixture("missing.avi"), "at": 0, "size": 64 }),
    ]);
    assert_eq!(replies.len(), 4);
    let (small, png) = &replies[0];
    assert_eq!(
        (
            small["ok"]["width"].as_u64(),
            small["ok"]["height"].as_u64()
        ),
        (Some(320), Some(240))
    );
    assert!(png.starts_with(b"\x89PNG"));
    // 720 by 576 of pixels 16:15 wide is 768 by 576, then fitted to 512.
    let (dvd, _) = &replies[1];
    assert_eq!(
        (dvd["ok"]["width"].as_u64(), dvd["ok"]["height"].as_u64()),
        (Some(512), Some(384))
    );
    let (h264, png) = &replies[2];
    if built_with("h264") {
        assert!(png.starts_with(b"\x89PNG"));
    } else {
        assert_eq!(h264["error"]["code"], "decoder");
        assert_eq!(h264["id"], 3);
    }
    assert_eq!(replies[3].0["error"]["code"], "open");
}

#[test]
fn a_request_it_cant_read_is_answered_and_the_rest_go_on() {
    let replies = serve(&[
        serde_json::json!({ "op": "spin" }),
        serde_json::json!({ "op": "caps", "id": 2 }),
    ]);
    assert_eq!(replies[0].0["error"]["code"], "request");
    assert_eq!(replies[1].0["id"], 2);
}

#[test]
fn a_stream_is_remuxed_with_its_sound_made_again() {
    let output = stream(&[
        "--input",
        &fixture("h264.mts"),
        "--video",
        "copy",
        "--audio",
        "transcode",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // A fragmented MP4, its picture as it was and its sound Opus.
    assert!(has(&output.stdout, "ftyp") && has(&output.stdout, "moof"));
    assert!(has(&output.stdout, "avc1") && has(&output.stdout, "Opus"));
}

#[test]
fn a_picture_is_made_again_as_vp9_where_asked() {
    let output = stream(&[
        "--input",
        &fixture("interlaced.vob"),
        "--video",
        "transcode",
        "--audio",
        "none",
        "--encoder",
        "vp9",
        "--start",
        "500",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(has(&output.stdout, "moof") && has(&output.stdout, "vp09"));
    assert!(!has(&output.stdout, "Opus"));
}

// Where the first fragment of the first track starts, in seconds: its tfdt
// over its mdhd's timescale.
fn first_fragment_at(bytes: &[u8]) -> f64 {
    let find = |name: &str| {
        bytes
            .windows(4)
            .position(|window| window == name.as_bytes())
            .expect(name)
    };
    let be = |at: usize, length: usize| {
        bytes[at..at + length]
            .iter()
            .fold(0u64, |value, &byte| value << 8 | u64::from(byte))
    };
    // mdhd: version and flags, two dates of four bytes (version 0), then the
    // timescale; tfdt: version and flags, then the time, of eight bytes in
    // version 1.
    let mdhd = find("mdhd") + 4;
    let timescale = be(mdhd + 12, 4);
    let tfdt = find("tfdt") + 4;
    let time = if bytes[tfdt] == 1 {
        be(tfdt + 4, 8)
    } else {
        be(tfdt + 4, 4)
    };
    time as f64 / timescale as f64
}

#[test]
fn a_stream_has_the_files_own_times_from_where_it_starts() {
    // A transport stream's times start well after zero; the stream's don't.
    let whole = stream(&[
        "--input",
        &fixture("h264.mts"),
        "--video",
        "copy",
        "--audio",
        "none",
    ]);
    assert!(whole.status.success());
    assert!(first_fragment_at(&whole.stdout) < 0.1);
    // From half a second in, from the keyframe before it, where it is in the
    // file: this one's are twelve frames apart, at 0.48 s. (Not the DVD's: a
    // program stream has no index, and in one of a second ffmpeg's seek goes
    // past it.)
    let later = stream(&[
        "--input",
        &fixture("mpeg4.avi"),
        "--video",
        "transcode",
        "--audio",
        "none",
        "--encoder",
        "vp9",
        "--start",
        "500",
    ]);
    assert!(later.status.success());
    let at = first_fragment_at(&later.stdout);
    assert!((0.3..=0.55).contains(&at), "it starts at {at}");
}

#[test]
fn a_stream_of_what_isnt_there_fails() {
    let output = stream(&["--input", &fixture("missing.avi")]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

fn render(args: &[&str]) -> Output {
    Command::new(PROGRAM)
        .arg("render")
        .args(args)
        .output()
        .expect("the program runs")
}

// A file of the test's own, in the system's temp folder, gone first.
fn scratch(name: &str) -> String {
    let path = std::env::temp_dir().join(format!("ffmpeg-rs-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path.to_string_lossy().into_owned()
}

// The width and height an MP4's sample entry of that type says, which come
// after its six reserved bytes, its data reference and sixteen bytes more.
// It's looked for in the sample descriptions, as the file's brands can have
// the same name.
fn picture_size(bytes: &[u8], entry: &str) -> Option<(u16, u16)> {
    let stsd = bytes.windows(4).position(|window| window == b"stsd")?;
    let at = stsd
        + bytes[stsd..]
            .windows(4)
            .position(|window| window == entry.as_bytes())?
        + 28;
    let field = |from: usize| {
        Some(u16::from_be_bytes([
            *bytes.get(from)?,
            *bytes.get(from + 1)?,
        ]))
    };
    Some((field(at)?, field(at + 2)?))
}

#[test]
fn a_rendition_is_square_fitted_and_kept_in_a_file() {
    for encoder in ["auto", "vp9"] {
        let path = scratch(&format!("dvd-{encoder}.mp4"));
        let output = render(&[
            "--input",
            &fixture("interlaced.vob"),
            "--output",
            &path,
            "--video",
            "transcode",
            "--audio",
            "transcode",
            "--encoder",
            encoder,
            "--max-size",
            "384",
            "--quality",
            "60",
        ]);
        assert!(
            output.status.success(),
            "{encoder}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = std::fs::read(&path).expect("the file is there");
        // Its index first, then what it indexes.
        let moov = bytes.windows(4).position(|window| window == b"moov");
        let mdat = bytes.windows(4).position(|window| window == b"mdat");
        assert!(moov < mdat, "{encoder}: moov after mdat");
        assert!(!has(&bytes, "moof") && has(&bytes, "Opus"));
        // H.264 by the machine's encoder, where it has one; VP9 where not.
        let entry = if encoder == "vp9" || !has(&bytes, "avc1") {
            "vp09"
        } else {
            "avc1"
        };
        // 720 by 576 of pixels 16:15 wide is 768 by 576, fitted to 384.
        assert_eq!(picture_size(&bytes, entry), Some((384, 288)), "{encoder}");
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn a_rendition_says_how_far_it_is() {
    let path = scratch("progress.mp4");
    let output = render(&[
        "--input",
        &fixture("mpeg4.avi"),
        "--output",
        &path,
        "--video",
        "copy",
        "--audio",
        "none",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON"))
        .collect();
    assert!(
        lines
            .iter()
            .all(|line| line["at"].as_u64().is_some_and(|at| at % 1000 == 0)),
        "{lines:?}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn render_and_stream_keep_to_their_own_options() {
    let to = scratch("unasked.mp4");
    let without = render(&["--input", &fixture("mpeg4.avi")]);
    assert!(!without.status.success());
    let started = render(&[
        "--input",
        &fixture("mpeg4.avi"),
        "--output",
        &to,
        "--start",
        "500",
    ]);
    assert!(!started.status.success());
    let streamed = stream(&["--input", &fixture("mpeg4.avi"), "--output", &to]);
    assert!(!streamed.status.success() && streamed.stdout.is_empty());
    let small = render(&[
        "--input",
        &fixture("mpeg4.avi"),
        "--output",
        &to,
        "--max-size",
        "8",
    ]);
    assert!(!small.status.success());
    assert!(!std::path::Path::new(&to).exists());
}
