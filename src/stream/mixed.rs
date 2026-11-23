//! A stream or a rendition with a picture or a sound this build has no
//! decoder for, or of a file the build can't read at all. What the build
//! can't decode is decoded by the system's reader, and what it can is the
//! build's, copied or made again as asked: a camcorder's H.264 by the
//! system and its AC-3 by the build, as a Windows without Dolby's decoder
//! has none for it. The two read the file side by side, whichever is behind
//! next, into the one MP4.

use ffmpeg_next::{Rational, Rescale, decoder, format, media};

use super::{
    Failure, Options, PictureSource, SoundSource, Way, audio_lane, feed, finish_lanes, keep_of,
    lanes_of, open_output, origin_of, progress, seek_to, shift, shifts_of, time_bases_of,
    video_lane,
};
use crate::system;

// Who decodes a picture or a sound, or copies it.
#[derive(Clone, Copy, PartialEq)]
enum By {
    Build,
    System,
    Neither,
}

// The build, where it reads the file and decodes the stream or copies it;
// the system where it can't; neither where it's not asked for or not there.
fn by(input: Option<&format::context::Input>, medium: media::Type, way: Way) -> By {
    if way == Way::None {
        return By::Neither;
    }
    let Some(input) = input else {
        return By::System;
    };
    match input.streams().best(medium) {
        None => By::Neither,
        Some(stream) if way == Way::Copy || decoder::find(stream.parameters().id()).is_some() => {
            By::Build
        }
        Some(_) => By::System,
    }
}

pub(super) fn system_stream(
    options: &Options,
    input: Option<format::context::Input>,
) -> Result<(), Failure> {
    let video_by = by(input.as_ref(), media::Type::Video, options.video);
    let audio_by = by(input.as_ref(), media::Type::Audio, options.audio);
    let mut reader = system::Reader::open(
        &options.input,
        video_by == By::System,
        audio_by == By::System,
    )
    .map_err(Failure::System)?;
    let (mut output, muxing) = open_output(options)?;
    let mut video = reader
        .picture
        .as_ref()
        .map(|picture| {
            video_lane(
                &mut output,
                &PictureSource {
                    width: picture.width,
                    height: picture.height,
                    pixels: format::Pixel::NV12,
                    time_base: system::TIME_BASE,
                    aspect: picture.aspect,
                    rate: picture.rate.unwrap_or(Rational(30, 1)),
                    turn: picture.turn,
                },
                None,
                options.encoder,
                keep_of(options),
            )
        })
        .transpose()?;
    let mut audio = reader
        .sound
        .as_ref()
        .map(|sound| {
            audio_lane(
                &mut output,
                &SoundSource {
                    rate: sound.rate,
                    format: format::Sample::F32(format::sample::Type::Packed),
                    layout: format!(
                        "0x{:x}",
                        ffmpeg_next::ChannelLayout::default(i32::from(sound.channels)).bits()
                    ),
                    time_base: system::TIME_BASE,
                },
                None,
            )
        })
        .transpose()?;

    // The build's part, where it has one: its lanes by the input's streams,
    // the system's left out.
    let build_way = |by: By, way: Way| if by == By::Build { way } else { Way::None };
    let mut build = input.filter(|_| video_by == By::Build || audio_by == By::Build);
    let mut lanes = match &build {
        Some(input) => lanes_of(
            input,
            &mut output,
            options,
            build_way(video_by, options.video),
            build_way(audio_by, options.audio),
        )?,
        None => Vec::new(),
    };
    if video.is_none() && audio.is_none() && lanes.is_empty() {
        return Err(Failure::System("it has nothing to make".into()));
    }

    let origin = build.as_ref().map_or(0, origin_of);
    if options.start_ms > 0 {
        reader.seek(options.start_ms).map_err(Failure::System)?;
        if let Some(input) = build.as_mut() {
            seek_to(input, options.start_ms, origin)?;
        }
    }
    let shifts = build
        .as_ref()
        .map_or_else(Vec::new, |input| shifts_of(input, origin));
    output.write_header_with(muxing)?;
    let time_bases = time_bases_of(&output);
    let video_to = video.as_ref().map(|lane| time_bases[lane.output]);
    let audio_to = audio.as_ref().map(|lane| time_bases[lane.output]);

    // How far into the file each side has read, in milliseconds from its
    // start, or None once it has ended. Both the system's times and the
    // build's, shifted by the origin, count from the file's start.
    let mut system_at = (video.is_some() || audio.is_some()).then_some(i64::MIN);
    let mut build_at = build.is_some().then_some(i64::MIN);
    let mut packets = build.as_mut().map(|input| input.packets());
    let keep = options.output.is_some();
    let mut said = 0;
    loop {
        let system_next = match (system_at, build_at) {
            (None, None) => break,
            (Some(system), Some(build)) => system <= build,
            (system, _) => system.is_some(),
        };
        if system_next {
            match reader.read().map_err(Failure::System)? {
                None => system_at = None,
                Some(system::Decoded::Video(frame)) => {
                    system_at = Some(frame.pts().unwrap_or(0) / 10_000);
                    if let (Some(lane), Some(to)) = (video.as_mut(), video_to) {
                        lane.push(&frame, &mut output, to)?;
                    }
                }
                Some(system::Decoded::Audio(frame)) => {
                    system_at = Some(frame.pts().unwrap_or(0) / 10_000);
                    if let (Some(lane), Some(to)) = (audio.as_mut(), audio_to) {
                        lane.push(&frame, &mut output, to)?;
                    }
                }
            }
        } else {
            match packets.as_mut().and_then(Iterator::next) {
                None => build_at = None,
                Some((stream, mut packet)) => {
                    shift(
                        &mut packet,
                        shifts.get(stream.index()).copied().unwrap_or(0),
                    );
                    if let Some(pts) = packet.pts() {
                        build_at = Some(pts.rescale(stream.time_base(), Rational(1, 1000)));
                    }
                    if let Some(lane) = lanes.get_mut(stream.index()) {
                        feed(lane, &mut packet, &mut output, &time_bases)?;
                    }
                }
            }
        }
        // As far as both have made, which the one behind says.
        if keep {
            if let Some(at) = [system_at, build_at].into_iter().flatten().min() {
                progress(&mut said, at);
            }
        }
    }

    if let (Some(lane), Some(to)) = (video.as_mut(), video_to) {
        lane.finish(&mut output, to)?;
    }
    if let (Some(lane), Some(to)) = (audio.as_mut(), audio_to) {
        lane.finish(&mut output, to)?;
    }
    finish_lanes(&mut lanes, &mut output, &time_bases)?;
    Ok(output.write_trailer()?)
}
