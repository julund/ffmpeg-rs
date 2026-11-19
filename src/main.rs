//! A small program over a trimmed, LGPL ffmpeg, for an app to start as a
//! process of its own: what this build decodes and makes, a frame of a video,
//! and a video remuxed or transcoded into a fragmented MP4 that a webview
//! plays. See README.md.
//!
//!   ffmpeg-rs serve       stays open; JSON lines in, JSON lines out (protocol.json)
//!   ffmpeg-rs stream ...  one video to stdout, until it ends or is killed
//!   ffmpeg-rs render ...  one video into an MP4 file to keep
//!   ffmpeg-rs --protocol  the JSON Schema of what serve takes and gives
//!   ffmpeg-rs --version   this program's version and ffmpeg's, and how ffmpeg was built

mod caps;
mod frame;
mod protocol;
mod stream;

use std::process::ExitCode;

fn main() -> ExitCode {
    if let Err(error) = ffmpeg_next::init() {
        eprintln!("ffmpeg didn't start: {error}");
        return ExitCode::FAILURE;
    }
    // ffmpeg says nothing on stderr but its errors, which the caller logs.
    ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => protocol::serve(),
        Some("stream") => stream::run(&args[1..]),
        Some("render") => stream::render(&args[1..]),
        Some("--protocol") => {
            print!("{}", protocol::SCHEMA);
            ExitCode::SUCCESS
        }
        Some("--version") => {
            println!(
                "{}",
                serde_json::to_string(&caps::version()).unwrap_or_default()
            );
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("Usage: ffmpeg-rs serve | stream ... | render ... | --protocol | --version");
            ExitCode::from(2)
        }
    }
}
