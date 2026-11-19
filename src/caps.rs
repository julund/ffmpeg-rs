//! What this build of ffmpeg has, from its own registry, so it can't say
//! other than what it was built with.

use std::ffi::{CStr, c_char, c_void};

use ffmpeg_next::ffi;
use serde::Serialize;

#[derive(Serialize)]
pub struct Caps {
    pub decoders: Vec<String>,
    pub encoders: Vec<String>,
    pub demuxers: Vec<String>,
    pub muxers: Vec<String>,
}

#[derive(Serialize)]
pub struct Version {
    pub program: &'static str,
    pub ffmpeg: String,
    /// The line ffmpeg was configured with, which its licence notice names.
    pub configuration: String,
    pub license: String,
}

fn text(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: ffmpeg's names and strings are static and end in a zero.
    unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

pub fn caps() -> Caps {
    let mut decoders = Vec::new();
    let mut encoders = Vec::new();
    let mut demuxers = Vec::new();
    let mut muxers = Vec::new();
    // SAFETY: the iterate functions walk ffmpeg's static tables with the
    // opaque cursor they're given, and return null at the end.
    unsafe {
        let mut cursor: *mut c_void = std::ptr::null_mut();
        loop {
            let codec = ffi::av_codec_iterate(&mut cursor);
            if codec.is_null() {
                break;
            }
            let name = text((*codec).name);
            if ffi::av_codec_is_decoder(codec) != 0 {
                decoders.push(name.clone());
            }
            if ffi::av_codec_is_encoder(codec) != 0 {
                encoders.push(name);
            }
        }
        let mut cursor: *mut c_void = std::ptr::null_mut();
        loop {
            let format = ffi::av_demuxer_iterate(&mut cursor);
            if format.is_null() {
                break;
            }
            demuxers.push(text((*format).name));
        }
        let mut cursor: *mut c_void = std::ptr::null_mut();
        loop {
            let format = ffi::av_muxer_iterate(&mut cursor);
            if format.is_null() {
                break;
            }
            muxers.push(text((*format).name));
        }
    }
    Caps {
        decoders,
        encoders,
        demuxers,
        muxers,
    }
}

pub fn version() -> Version {
    // SAFETY: these return static strings.
    unsafe {
        Version {
            program: env!("CARGO_PKG_VERSION"),
            ffmpeg: text(ffi::av_version_info()),
            configuration: text(ffi::avcodec_configuration()),
            license: text(ffi::avcodec_license()),
        }
    }
}
