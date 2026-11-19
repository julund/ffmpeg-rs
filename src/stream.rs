//! One video into a fragmented MP4 on stdout, which a webview plays as it
//! comes: each of its first picture and first sound copied as it is, or
//! made again, from a time on, until it ends or the program is killed. Or,
//! with render, into an MP4 file to keep.
//!
//!   ffmpeg-rs stream --input <path> [--start <ms>] [--video copy|transcode]
//!     [--audio copy|transcode|none] [--encoder auto|h264|vp9]
//!   ffmpeg-rs render --input <path> --output <path> [--max-size <pixels>]
//!     [--quality 1-100] and stream's --video, --audio and --encoder
//!
//! A picture is made again as H.264 by Windows' own encoder where the
//! machine has one, and as VP9 where it hasn't, deinterlaced and with square
//! pixels; a sound as Opus. All of them play in an MP4. --encoder vp9 makes
//! it VP9 where there is a hardware encoder too, as a machine without one
//! would, and --encoder h264 fails where there's none.
//!
//! render makes it to keep rather than to watch at once: H.264 by Windows'
//! software encoder where there's no hardware one, before VP9, which it makes
//! a little smaller than stream's, at the quality asked. The picture fits inside
//! --max-size, never made larger, at --quality (75 where it isn't said). Its
//! file has its index first, to play as it's downloaded. It writes a line on
//! stdout as each second of the video is made, {"at":<ms>}, from the file's
//! start; a file that's left by one that failed or was killed is half made.

use std::io::Write;
use std::process::ExitCode;

use ffmpeg_next::{
    Dictionary, Packet, Rational, Rescale, codec, decoder, encoder, filter, format, frame, media,
};

use crate::system;

const MICROSECONDS: Rational = Rational(1, 1_000_000);

#[derive(Clone, Copy, PartialEq)]
enum Way {
    Copy,
    Transcode,
    None,
}

// Which encoder a picture is made again by.
#[derive(Clone, Copy, PartialEq)]
enum Encoder {
    Auto,
    H264,
    Vp9,
}

struct Options {
    input: String,
    // A file to keep, rather than a stream on stdout.
    output: Option<String>,
    start_ms: i64,
    video: Way,
    audio: Way,
    encoder: Encoder,
    max_size: Option<u32>,
    quality: u8,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        input: String::new(),
        output: None,
        start_ms: 0,
        video: Way::Copy,
        audio: Way::Copy,
        encoder: Encoder::Auto,
        max_size: None,
        quality: 75,
    };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(format!("{flag} needs a value"))?;
        let way = |value: &str| match value {
            "copy" => Ok(Way::Copy),
            "transcode" => Ok(Way::Transcode),
            "none" => Ok(Way::None),
            _ => Err(format!("{value} is no way of making a stream")),
        };
        match flag.as_str() {
            "--input" => options.input = value.clone(),
            "--output" => options.output = Some(value.clone()),
            "--start" => options.start_ms = value.parse().map_err(|_| "--start is milliseconds")?,
            "--max-size" => {
                options.max_size = Some(
                    value
                        .parse()
                        .ok()
                        .filter(|size| *size >= 16)
                        .ok_or("--max-size is 16 pixels or more")?,
                )
            }
            "--quality" => {
                options.quality = value
                    .parse()
                    .ok()
                    .filter(|quality| (1..=100).contains(quality))
                    .ok_or("--quality is 1 to 100")?
            }
            "--video" => options.video = way(value)?,
            "--audio" => options.audio = way(value)?,
            "--encoder" => {
                options.encoder = match value.as_str() {
                    "auto" => Encoder::Auto,
                    "h264" => Encoder::H264,
                    "vp9" => Encoder::Vp9,
                    _ => return Err(format!("{value} is no encoder")),
                }
            }
            _ => return Err(format!("{flag} isn't an option")),
        }
    }
    if options.input.is_empty() {
        return Err("--input is needed".into());
    }
    Ok(options)
}

pub fn run(args: &[String]) -> ExitCode {
    finish(options(args).and_then(|options| {
        if options.output.is_some() || options.max_size.is_some() {
            return Err("--output and --max-size are render's".into());
        }
        stream(&options).map_err(|error| error.to_string())
    }))
}

pub fn render(args: &[String]) -> ExitCode {
    finish(options(args).and_then(|options| {
        if options.output.is_none() {
            return Err("--output is needed".into());
        }
        if options.start_ms != 0 {
            return Err("--start is stream's".into());
        }
        stream(&options).map_err(|error| error.to_string())
    }))
}

// What a stream or a rendition fails by: ffmpeg's error, or the system's.
enum Failure {
    Ffmpeg(ffmpeg_next::Error),
    System(String),
}

impl From<ffmpeg_next::Error> for Failure {
    fn from(error: ffmpeg_next::Error) -> Self {
        Failure::Ffmpeg(error)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Failure::Ffmpeg(error) => error.fmt(formatter),
            Failure::System(error) => formatter.write_str(error),
        }
    }
}

fn finish(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

// What becomes of an input stream.
enum Lane {
    Skip,
    Copy {
        output: usize,
        time_base: Rational,
        last: Option<i64>,
    },
    Video(Box<VideoLane>),
    Audio(Box<AudioLane>),
}

// Each lane keeps the time of the last packet it wrote (last), in its
// output's time base. Its decoder is ffmpeg's; one with none is given the
// system's frames.
struct VideoLane {
    output: usize,
    decoder: Option<decoder::Video>,
    graph: filter::Graph,
    encoder: encoder::Video,
    last: Option<i64>,
}

struct AudioLane {
    output: usize,
    decoder: Option<decoder::Audio>,
    graph: filter::Graph,
    encoder: encoder::Audio,
    last: Option<i64>,
    // The input stream's time base, a second in it, and the time the next
    // decoded frame starts at, in it.
    time_base: Rational,
    second: i64,
    next: Option<i64>,
}

// Whether a stream to be made again has a codec this build has no decoder
// for, which the system's decoders are for.
fn needs_system(input: &format::context::Input, options: &Options) -> bool {
    [
        (media::Type::Video, options.video),
        (media::Type::Audio, options.audio),
    ]
    .into_iter()
    .any(|(medium, way)| {
        way == Way::Transcode
            && input
                .streams()
                .best(medium)
                .is_some_and(|stream| decoder::find(stream.parameters().id()).is_none())
    })
}

// The MP4 a stream or a rendition is written into, and how it's muxed.
// A stream is fragments from the first keyframe on, with no index at the
// end to wait for. Their times are the file's own, from its start, wherever
// the stream starts in it, so that a stream from a time is where a player
// has it: only a time before the start, as a B-frame's at it can be, is
// moved up.
fn open_output(
    options: &Options,
) -> Result<(format::context::Output, Dictionary<'static>), ffmpeg_next::Error> {
    let mut muxing = Dictionary::new();
    muxing.set("avoid_negative_ts", "make_non_negative");
    let output = match &options.output {
        Some(path) => {
            // The index before the media, moved there once it's all written.
            muxing.set("movflags", "+faststart");
            format::output_as(path, "mp4")?
        }
        None => {
            // frag_discont has a fragment's time be its first frame's, which
            // the muxer would otherwise take to be zero.
            muxing.set(
                "movflags",
                "frag_keyframe+empty_moov+default_base_moof+frag_discont",
            );
            format::output_as("pipe:1", "mp4")?
        }
    };
    Ok((output, muxing))
}

fn keep_of(options: &Options) -> Option<Keep> {
    options.output.is_some().then_some(Keep {
        max_size: options.max_size,
        quality: options.quality,
    })
}

// Says on stdout how far a file to keep is, each whole second; said is the
// last it said.
fn progress(said: &mut i64, ms: i64) {
    if ms >= *said + 1000 {
        *said = ms / 1000 * 1000;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, r#"{{"at":{said}}}"#);
        let _ = out.flush();
    }
}

fn stream(options: &Options) -> Result<(), Failure> {
    // What this build can't decode is decoded by the system, as is a file
    // the build can't read at all; the system's reader does all of it then.
    let transcoding = options.video == Way::Transcode || options.audio == Way::Transcode;
    let mut input = match format::input(&options.input) {
        Ok(input) if needs_system(&input, options) => return system_stream(options),
        Ok(input) => input,
        Err(_) if transcoding => return system_stream(options),
        Err(error) => return Err(error.into()),
    };
    let video = input
        .streams()
        .best(media::Type::Video)
        .map(|stream| stream.index());
    let audio = input
        .streams()
        .best(media::Type::Audio)
        .map(|stream| stream.index());

    let (mut output, muxing) = open_output(options)?;
    let keep = options.output.is_some();
    // Where the file's own times start, as a camcorder's stream's don't at
    // zero; in microseconds.
    // SAFETY: the context is the input's, open.
    let origin = match unsafe { (*input.as_ptr()).start_time } {
        ffmpeg_next::ffi::AV_NOPTS_VALUE => 0,
        start => start,
    };

    let mut lanes: Vec<Lane> = Vec::new();
    for stream in input.streams() {
        let way = if Some(stream.index()) == video {
            options.video
        } else if Some(stream.index()) == audio {
            options.audio
        } else {
            Way::None
        };
        let medium = stream.parameters().medium();
        let lane = match (way, medium) {
            (Way::None, _) => Lane::Skip,
            (Way::Copy, _) => {
                let mut added = output.add_stream(encoder::find(codec::Id::None))?;
                added.set_parameters(stream.parameters());
                // The tag of the container it came from means nothing in an MP4.
                // SAFETY: the parameters are the stream's own, just set.
                unsafe { (*added.parameters().as_mut_ptr()).codec_tag = 0 };
                Lane::Copy {
                    output: added.index(),
                    time_base: stream.time_base(),
                    last: None,
                }
            }
            (Way::Transcode, media::Type::Video) => {
                let mut decoding = codec::context::Context::from_parameters(stream.parameters())?;
                decoding.set_threading(threads());
                let decoder = decoding.decoder().video()?;
                let source = PictureSource {
                    width: decoder.width(),
                    height: decoder.height(),
                    pixels: decoder.format(),
                    time_base: stream.time_base(),
                    aspect: match decoder.aspect_ratio() {
                        aspect if aspect.numerator() > 0 => aspect,
                        _ => Rational(1, 1),
                    },
                    rate: match stream.avg_frame_rate() {
                        rate if rate.numerator() > 0 => rate,
                        _ => Rational(25, 1),
                    },
                    turn: turn_of(&stream),
                };
                Lane::Video(Box::new(video_lane(
                    &mut output,
                    &source,
                    Some(decoder),
                    options.encoder,
                    keep_of(options),
                )?))
            }
            (Way::Transcode, media::Type::Audio) => {
                let decoder = codec::context::Context::from_parameters(stream.parameters())?
                    .decoder()
                    .audio()?;
                let source = SoundSource {
                    rate: decoder.rate(),
                    format: decoder.format(),
                    layout: layout_mask(&decoder),
                    time_base: stream.time_base(),
                };
                Lane::Audio(Box::new(audio_lane(&mut output, &source, Some(decoder))?))
            }
            (Way::Transcode, _) => Lane::Skip,
        };
        lanes.push(lane);
    }

    if options.start_ms > 0 {
        let target = options.start_ms * i64::from(ffmpeg_next::ffi::AV_TIME_BASE) / 1000 + origin;
        input.seek(target, ..target)?;
    }
    // The start in each stream's own time base, which its times are taken from.
    let shifts: Vec<i64> = input
        .streams()
        .map(|stream| origin.rescale(MICROSECONDS, stream.time_base()))
        .collect();
    output.write_header_with(muxing)?;
    let time_bases: Vec<Rational> = (0..output.nb_streams() as usize)
        .map(|index| {
            output
                .stream(index)
                .map(|stream| stream.time_base())
                .unwrap_or(Rational(1, 1))
        })
        .collect();

    // The last whole second a file to keep said it had made.
    let mut said = 0;
    for (stream, mut packet) in input.packets() {
        let Some(lane) = lanes.get_mut(stream.index()) else {
            continue;
        };
        let shift = shifts.get(stream.index()).copied().unwrap_or(0);
        packet.set_pts(packet.pts().map(|pts| pts - shift));
        packet.set_dts(packet.dts().map(|dts| dts - shift));
        if keep && Some(stream.index()) == video.or(audio) {
            if let Some(at) = packet.pts() {
                progress(&mut said, at.rescale(stream.time_base(), Rational(1, 1000)));
            }
        }
        match lane {
            Lane::Skip => {}
            Lane::Copy {
                output: index,
                time_base,
                last,
            } => {
                packet.rescale_ts(*time_base, time_bases[*index]);
                packet.set_position(-1);
                packet.set_stream(*index);
                write_packet(&mut packet, last, &mut output)?;
            }
            Lane::Video(lane) => {
                // A packet that can't be decoded is passed over, as a player
                // would.
                let sent = lane
                    .decoder
                    .as_mut()
                    .is_some_and(|decoder| decoder.send_packet(&packet).is_ok());
                if sent {
                    lane.drain(&mut output, time_bases[lane.output])?;
                }
            }
            Lane::Audio(lane) => {
                let sent = lane
                    .decoder
                    .as_mut()
                    .is_some_and(|decoder| decoder.send_packet(&packet).is_ok());
                if sent {
                    lane.drain(&mut output, time_bases[lane.output])?;
                }
            }
        }
    }
    for lane in &mut lanes {
        match lane {
            Lane::Video(lane) => {
                if let Some(decoder) = lane.decoder.as_mut() {
                    let _ = decoder.send_eof();
                }
                lane.drain(&mut output, time_bases[lane.output])?;
                lane.finish(&mut output, time_bases[lane.output])?;
            }
            Lane::Audio(lane) => {
                if let Some(decoder) = lane.decoder.as_mut() {
                    let _ = decoder.send_eof();
                }
                lane.drain(&mut output, time_bases[lane.output])?;
                lane.finish(&mut output, time_bases[lane.output])?;
            }
            _ => {}
        }
    }
    Ok(output.write_trailer()?)
}

// A stream or a rendition decoded by the system's reader: its picture and
// its sound each made again, as there's nothing of the file's own to copy.
fn system_stream(options: &Options) -> Result<(), Failure> {
    let mut reader = system::Reader::open(
        &options.input,
        options.video != Way::None,
        options.audio != Way::None,
    )
    .map_err(Failure::System)?;
    let (mut output, muxing) = open_output(options)?;
    let mut video = match &reader.picture {
        Some(picture) => Some(video_lane(
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
        )?),
        None => None,
    };
    let mut audio = match &reader.sound {
        Some(sound) => Some(audio_lane(
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
        )?),
        None => None,
    };
    if video.is_none() && audio.is_none() {
        return Err(Failure::System("it has nothing to make".into()));
    }
    if options.start_ms > 0 {
        reader.seek(options.start_ms).map_err(Failure::System)?;
    }
    output.write_header_with(muxing)?;
    let time_base = |index: usize| {
        output
            .stream(index)
            .map(|stream| stream.time_base())
            .unwrap_or(Rational(1, 1))
    };
    let video_to = video.as_ref().map(|lane| time_base(lane.output));
    let audio_to = audio.as_ref().map(|lane| time_base(lane.output));
    let keep = options.output.is_some();
    let mut said = 0;
    while let Some(decoded) = reader.read().map_err(Failure::System)? {
        match decoded {
            system::Decoded::Video(frame) => {
                if let (Some(lane), Some(to)) = (video.as_mut(), video_to) {
                    if keep {
                        progress(&mut said, frame.pts().unwrap_or(0) / 10_000);
                    }
                    lane.push(&frame, &mut output, to)?;
                }
            }
            system::Decoded::Audio(frame) => {
                if let (Some(lane), Some(to)) = (audio.as_mut(), audio_to) {
                    if keep && video.is_none() {
                        progress(&mut said, frame.pts().unwrap_or(0) / 10_000);
                    }
                    lane.push(&frame, &mut output, to)?;
                }
            }
        }
    }
    if let (Some(lane), Some(to)) = (video.as_mut(), video_to) {
        lane.finish(&mut output, to)?;
    }
    if let (Some(lane), Some(to)) = (audio.as_mut(), audio_to) {
        lane.finish(&mut output, to)?;
    }
    Ok(output.write_trailer()?)
}

// How far a picture is to be turned clockwise to be upright, by its stream's
// display matrix, in quarter turns: as ffmpeg's own command turns it.
fn turn_of(stream: &format::stream::Stream) -> u32 {
    // SAFETY: the stream's parameters and their side data are the input's,
    // open; the matrix is nine 32-bit numbers where there's one.
    let degrees = unsafe {
        let parameters = stream.parameters().as_ptr();
        let data = ffmpeg_next::ffi::av_packet_side_data_get(
            (*parameters).coded_side_data,
            (*parameters).nb_coded_side_data,
            ffmpeg_next::ffi::AVPacketSideDataType::AV_PKT_DATA_DISPLAYMATRIX,
        );
        if data.is_null() || (*data).size < 36 {
            return 0;
        }
        -ffmpeg_next::ffi::av_display_rotation_get((*data).data as *const i32)
    };
    quarter_turn(degrees)
}

// A turn in degrees, either way, as the nearest quarter turn clockwise.
fn quarter_turn(degrees: f64) -> u32 {
    if !degrees.is_finite() {
        return 0;
    }
    ((degrees / 90.0).round() as i64).rem_euclid(4) as u32 * 90
}

fn graph(
    source_name: &str,
    source: &str,
    sink_name: &str,
    spec: &str,
) -> Result<filter::Graph, ffmpeg_next::Error> {
    let mut graph = filter::Graph::new();
    graph.add(
        &filter::find(source_name).ok_or(ffmpeg_next::Error::FilterNotFound)?,
        "in",
        source,
    )?;
    graph.add(
        &filter::find(sink_name).ok_or(ffmpeg_next::Error::FilterNotFound)?,
        "out",
        "",
    )?;
    graph.output("in", 0)?.input("out", 0)?.parse(spec)?;
    graph.validate()?;
    Ok(graph)
}

// How a picture to keep is made: the size it fits inside, and its quality.
#[derive(Clone, Copy)]
struct Keep {
    max_size: Option<u32>,
    quality: u8,
}

// The size a picture is made at: its pixels made square, then fitted inside
// max_size where it's larger, each side even, as the encoders take it.
fn made_size(width: u32, height: u32, aspect: f64, max_size: Option<u32>) -> (u32, u32) {
    let wide = (f64::from(width) * aspect).max(2.0);
    let high = f64::from(height).max(2.0);
    let scale = max_size.map_or(1.0, |max| (f64::from(max) / wide.max(high)).min(1.0));
    let even = |side: f64| ((side * scale / 2.0).round() as u32 * 2).max(2);
    (even(wide), even(high))
}

// A thread for each of the machine's cores, as ffmpeg's own command asks
// for: a codec's context is given one otherwise, which made a 1080p clip's
// VP9 take more than twice as long.
fn threads() -> ffmpeg_next::codec::threading::Config {
    ffmpeg_next::codec::threading::Config {
        kind: ffmpeg_next::codec::threading::Type::Frame,
        count: std::thread::available_parallelism().map_or(1, usize::from),
        ..Default::default()
    }
}

// An encoder to try, the pixels it takes and its settings.
type Choice = (ffmpeg_next::Codec, format::Pixel, Dictionary<'static>);

// The encoders a picture is tried by, in order. As it's watched: Windows'
// hardware H.264, then VP9 as fast as it goes. To keep: the hardware's, then
// Windows' software H.264, then VP9 at the quality asked. h264 alone has no
// VP9 after it.
fn choices(choice: Encoder, keep: Option<Keep>) -> Result<Vec<Choice>, ffmpeg_next::Error> {
    let mut choices = Vec::new();
    if choice != Encoder::Vp9 {
        if let Some(codec) = encoder::find_by_name("h264_mf") {
            let quality = keep.map(|keep| keep.quality);
            choices.push((codec, format::Pixel::NV12, h264_settings(true, quality)));
            if let Some(quality) = quality {
                choices.push((
                    codec,
                    format::Pixel::NV12,
                    h264_settings(false, Some(quality)),
                ));
            }
        }
    }
    if choice != Encoder::H264 {
        choices.push((
            vp9()?,
            format::Pixel::YUV420P,
            match keep {
                Some(keep) => vp9_kept(keep.quality),
                None => vp9_settings(),
            },
        ));
    }
    if choices.is_empty() {
        return Err(ffmpeg_next::Error::EncoderNotFound);
    }
    Ok(choices)
}

// Media Foundation's H.264, by the machine's hardware or Windows' software;
// at a quality, from 1 to 100, which its rate control goes by.
fn h264_settings(hardware: bool, quality: Option<u8>) -> Dictionary<'static> {
    let mut settings = Dictionary::new();
    settings.set("hw_encoding", if hardware { "1" } else { "0" });
    settings.set("rate_control", "quality");
    if let Some(quality) = quality {
        settings.set("quality", &quality.to_string());
    }
    settings
}

// The frames a picture's lane is given: their size, pixels and time base,
// the pixels' shape, the frame rate, and how far they're to be turned
// clockwise to be upright, in degrees.
struct PictureSource {
    width: u32,
    height: u32,
    pixels: format::Pixel,
    time_base: Rational,
    aspect: Rational,
    rate: Rational,
    turn: u32,
}

// The filters that turn a picture clockwise by a quarter turn or more.
fn turning(turn: u32) -> &'static str {
    match turn {
        90 => ",transpose=clock",
        180 => ",hflip,vflip",
        270 => ",transpose=cclock",
        _ => "",
    }
}

fn video_lane(
    output: &mut format::context::Output,
    from: &PictureSource,
    decoder: Option<decoder::Video>,
    choice: Encoder,
    keep: Option<Keep>,
) -> Result<VideoLane, ffmpeg_next::Error> {
    let global = output
        .format()
        .flags()
        .contains(format::Flags::GLOBAL_HEADER);

    let (scaled_width, scaled_height) = made_size(
        from.width,
        from.height,
        f64::from(from.aspect),
        keep.and_then(|keep| keep.max_size),
    );
    // Turned a quarter, it's as high as it was wide.
    let (width, height) = if from.turn % 180 == 90 {
        (scaled_height, scaled_width)
    } else {
        (scaled_width, scaled_height)
    };
    let rate = from.rate;
    let spec = |pixels: format::Pixel| {
        format!(
            // A frame for each frame, not one for each field.
            "bwdif=mode=send_frame:deint=interlaced,scale={scaled_width}:{scaled_height},setsar=1{},format={}",
            turning(from.turn),
            pixels
                .descriptor()
                .map(|descriptor| descriptor.name())
                .unwrap_or("yuv420p")
        )
    };
    let source = format!(
        "video_size={}x{}:pix_fmt={}:time_base={}/{}:pixel_aspect={}/{}",
        from.width,
        from.height,
        ffmpeg_next::ffi::AVPixelFormat::from(from.pixels) as i32,
        from.time_base.numerator(),
        from.time_base.denominator(),
        from.aspect.numerator(),
        from.aspect.denominator(),
    );
    // The graph for the encoder's pixels, and the encoder at the time base
    // the graph gives its frames in: bwdif halves it, so as to have room
    // for a frame of each field, whichever it sends.
    let open = |codec: ffmpeg_next::Codec, pixels: format::Pixel, settings: Dictionary| {
        let mut graph = graph("buffer", &source, "buffersink", &spec(pixels))?;
        let shown = sink_time_base(&mut graph)?;
        let mut context = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()?;
        context.set_width(width);
        context.set_height(height);
        context.set_format(pixels);
        context.set_frame_rate(Some(rate));
        context.set_time_base(shown);
        context.set_threading(threads());
        if global {
            context.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        Ok::<_, ffmpeg_next::Error>((graph, context.open_as_with(codec, settings)?))
    };
    // A machine without the hardware has the encoder but can't open it: the
    // next is tried, and the last one's error said.
    let mut opened = Err(ffmpeg_next::Error::EncoderNotFound);
    for (codec, pixels, settings) in choices(choice, keep)? {
        opened = open(codec, pixels, settings);
        if opened.is_ok() {
            break;
        }
    }
    let (graph, encoder) = opened?;
    let mut added = output.add_stream(encoder.codec())?;
    added.set_parameters(&encoder);
    added.set_time_base(encoder.time_base());
    Ok(VideoLane {
        output: added.index(),
        graph,
        decoder,
        encoder,
        last: None,
    })
}

// The time base of the frames a graph gives out.
fn sink_time_base(graph: &mut filter::Graph) -> Result<Rational, ffmpeg_next::Error> {
    let sink = graph.get("out").ok_or(ffmpeg_next::Error::FilterNotFound)?;
    // SAFETY: the sink is a buffersink of a configured graph, which is what
    // av_buffersink_get_time_base asks for.
    Ok(Rational::from(unsafe {
        ffmpeg_next::ffi::av_buffersink_get_time_base(sink.as_ptr())
    }))
}

fn vp9() -> Result<ffmpeg_next::Codec, ffmpeg_next::Error> {
    encoder::find_by_name("libvpx-vp9").ok_or(ffmpeg_next::Error::EncoderNotFound)
}

// As fast as libvpx goes, for a picture made as it's watched.
fn vp9_settings() -> Dictionary<'static> {
    let mut settings = Dictionary::new();
    settings.set("deadline", "realtime");
    settings.set("cpu-used", "8");
    settings.set("row-mt", "1");
    settings.set("lag-in-frames", "0");
    settings.set("crf", "32");
    settings.set("b", "0");
    settings
}

// To keep: libvpx's realtime setting a step slower than stream's, and its
// constant quality from the quality asked for, 100 being crf 15, 75 crf 27
// and 1 crf 63, its least. Its good setting made a 1080p clip 13 % smaller
// in four times as long (a 10 s clip on an i7-4790K: 13.2 s against 3.0 s),
// three times slower than it plays, which a library's videos can't wait for.
fn vp9_kept(quality: u8) -> Dictionary<'static> {
    let crf = 15 + (100 - u32::from(quality)) * 48 / 99;
    let mut settings = Dictionary::new();
    settings.set("deadline", "realtime");
    settings.set("cpu-used", "6");
    settings.set("row-mt", "1");
    settings.set("crf", &crf.to_string());
    settings.set("b", "0");
    settings
}

#[cfg(test)]
mod tests {
    use super::made_size;

    #[test]
    fn a_turn_is_the_nearest_quarter_clockwise() {
        use super::quarter_turn;
        assert_eq!(quarter_turn(90.0), 90);
        assert_eq!(quarter_turn(-90.0), 270);
        assert_eq!(quarter_turn(180.0), 180);
        assert_eq!(quarter_turn(-180.0), 180);
        assert_eq!(quarter_turn(89.4), 90);
        assert_eq!(quarter_turn(0.0), 0);
        assert_eq!(quarter_turn(f64::NAN), 0);
    }

    #[test]
    fn a_picture_is_made_square_and_fitted_never_larger() {
        assert_eq!(made_size(640, 480, 1.0, None), (640, 480));
        // A DVD's 720 by 576 of pixels 16:15 wide.
        assert_eq!(made_size(720, 576, 16.0 / 15.0, None), (768, 576));
        assert_eq!(made_size(720, 576, 16.0 / 15.0, Some(384)), (384, 288));
        assert_eq!(made_size(320, 240, 1.0, Some(1920)), (320, 240));
        assert_eq!(made_size(1080, 1920, 1.0, Some(1280)), (720, 1280));
    }
}

// The frames a sound's lane is given: their rate, sample format, channels as
// abuffer's mask, and time base.
struct SoundSource {
    rate: u32,
    format: format::Sample,
    layout: String,
    time_base: Rational,
}

fn audio_lane(
    output: &mut format::context::Output,
    from: &SoundSource,
    decoder: Option<decoder::Audio>,
) -> Result<AudioLane, ffmpeg_next::Error> {
    let codec = encoder::find_by_name("libopus").ok_or(ffmpeg_next::Error::EncoderNotFound)?;
    let mut context = codec::context::Context::new_with_codec(codec)
        .encoder()
        .audio()?;
    context.set_rate(48_000);
    context.set_channel_layout(ffmpeg_next::ChannelLayout::STEREO);
    context.set_format(format::Sample::F32(format::sample::Type::Packed));
    context.set_bit_rate(128_000);
    context.set_time_base((1, 48_000));
    if output
        .format()
        .flags()
        .contains(format::Flags::GLOBAL_HEADER)
    {
        context.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    let encoder = context.open_as(codec)?;
    let mut added = output.add_stream(codec)?;
    added.set_parameters(&encoder);
    added.set_time_base((1, 48_000));

    let time_base = from.time_base;
    let source = format!(
        "time_base={}/{}:sample_rate={}:sample_fmt={}:channel_layout={}",
        time_base.numerator(),
        time_base.denominator(),
        from.rate,
        from.format.name(),
        from.layout,
    );
    let mut graph = graph(
        "abuffer",
        &source,
        "abuffersink",
        "aresample=48000,aformat=sample_fmts=flt:sample_rates=48000:channel_layouts=stereo",
    )?;
    if let Some(mut sink) = graph.get("out") {
        sink.sink().set_frame_size(encoder.frame_size());
    }
    Ok(AudioLane {
        output: added.index(),
        decoder,
        graph,
        encoder,
        last: None,
        second: 1.rescale(Rational(1, 1), time_base),
        time_base,
        next: None,
    })
}

// The decoder's channels as the mask abuffer takes, "0x3" for stereo; a
// sound that doesn't say which channels it has is given the usual ones for
// as many.
fn layout_mask(decoder: &decoder::Audio) -> String {
    let layout = match decoder.channel_layout() {
        layout if !layout.is_empty() => layout,
        _ => ffmpeg_next::ChannelLayout::default(i32::from(decoder.channels())),
    };
    format!("0x{:x}", layout.bits())
}

// Writes a packet unless its time is not after the last one's of its stream,
// which the muxer would refuse, ending the whole stream over one packet: a
// DVD's sound times only some of its frames, and its times can overlap.
fn write_packet(
    packet: &mut Packet,
    last: &mut Option<i64>,
    output: &mut format::context::Output,
) -> Result<(), ffmpeg_next::Error> {
    if let (Some(dts), Some(before)) = (packet.dts(), *last) {
        if dts <= before {
            return Ok(());
        }
    }
    *last = packet.dts().or(*last);
    packet.write_interleaved(output)
}

// Writes what an encoder has made.
fn write(
    encoder: &mut encoder::encoder::Encoder,
    from: Rational,
    index: usize,
    to: Rational,
    output: &mut format::context::Output,
    last: &mut Option<i64>,
) -> Result<(), ffmpeg_next::Error> {
    let mut packet = Packet::empty();
    while encoder.receive_packet(&mut packet).is_ok() {
        packet.set_stream(index);
        packet.rescale_ts(from, to);
        write_packet(&mut packet, last, output)?;
    }
    Ok(())
}

impl VideoLane {
    // Decoded frames through the filters into the encoder, and what it made
    // out.
    fn drain(
        &mut self,
        output: &mut format::context::Output,
        to: Rational,
    ) -> Result<(), ffmpeg_next::Error> {
        let Some(mut decoder) = self.decoder.take() else {
            return Ok(());
        };
        let mut decoded = frame::Video::empty();
        let mut result = Ok(());
        while decoder.receive_frame(&mut decoded).is_ok() {
            let timestamp = decoded.timestamp();
            decoded.set_pts(timestamp);
            result = self.push(&decoded, output, to);
            if result.is_err() {
                break;
            }
        }
        self.decoder = Some(decoder);
        result
    }

    // A frame, timed, through the filters into the encoder.
    fn push(
        &mut self,
        frame: &frame::Video,
        output: &mut format::context::Output,
        to: Rational,
    ) -> Result<(), ffmpeg_next::Error> {
        if let Some(mut source) = self.graph.get("in") {
            source.source().add(frame)?;
        }
        self.pull(output, to)
    }

    fn pull(
        &mut self,
        output: &mut format::context::Output,
        to: Rational,
    ) -> Result<(), ffmpeg_next::Error> {
        let mut shown = frame::Video::empty();
        while let Some(mut sink) = self.graph.get("out") {
            if sink.sink().frame(&mut shown).is_err() {
                break;
            }
            self.encoder.send_frame(&shown)?;
            let from = self.encoder.time_base();
            write(
                &mut self.encoder,
                from,
                self.output,
                to,
                output,
                &mut self.last,
            )?;
        }
        Ok(())
    }

    fn finish(
        &mut self,
        output: &mut format::context::Output,
        to: Rational,
    ) -> Result<(), ffmpeg_next::Error> {
        if let Some(mut source) = self.graph.get("in") {
            source.source().flush()?;
        }
        self.pull(output, to)?;
        self.encoder.send_eof()?;
        let from = self.encoder.time_base();
        write(
            &mut self.encoder,
            from,
            self.output,
            to,
            output,
            &mut self.last,
        )
    }
}

impl AudioLane {
    // Each frame's time, from the first's on by how many samples came before
    // it, rather than the times the decoder gives: a DVD's AC-3 has some of
    // its frames timed and the rest not, which comes out as times that
    // overlap. A jump of more than a second, as after a gap in the file, is
    // taken as it is.
    fn time_of(&mut self, decoded: &frame::Audio) -> Option<i64> {
        let said = decoded.timestamp();
        let pts = match (self.next, said) {
            (Some(next), Some(said)) if (said - next).abs() <= self.second => next,
            (Some(next), None) => next,
            (_, said) => said?,
        };
        let samples = i64::try_from(decoded.samples()).unwrap_or(0);
        self.next = Some(pts + samples.rescale(Rational(1, decoded.rate() as i32), self.time_base));
        Some(pts)
    }

    fn drain(
        &mut self,
        output: &mut format::context::Output,
        to: Rational,
    ) -> Result<(), ffmpeg_next::Error> {
        let Some(mut decoder) = self.decoder.take() else {
            return Ok(());
        };
        let mut decoded = frame::Audio::empty();
        let mut result = Ok(());
        while decoder.receive_frame(&mut decoded).is_ok() {
            let timestamp = self.time_of(&decoded);
            decoded.set_pts(timestamp);
            result = self.push(&decoded, output, to);
            if result.is_err() {
                break;
            }
        }
        self.decoder = Some(decoder);
        result
    }

    // A frame, timed, through the filters into the encoder. The system's are
    // timed as they come, without the gaps ffmpeg's decoders leave.
    fn push(
        &mut self,
        frame: &frame::Audio,
        output: &mut format::context::Output,
        to: Rational,
    ) -> Result<(), ffmpeg_next::Error> {
        if let Some(mut source) = self.graph.get("in") {
            source.source().add(frame)?;
        }
        self.pull(output, to)
    }

    fn pull(
        &mut self,
        output: &mut format::context::Output,
        to: Rational,
    ) -> Result<(), ffmpeg_next::Error> {
        let mut sound = frame::Audio::empty();
        while let Some(mut sink) = self.graph.get("out") {
            if sink.sink().frame(&mut sound).is_err() {
                break;
            }
            self.encoder.send_frame(&sound)?;
            let from = self.encoder.time_base();
            write(
                &mut self.encoder,
                from,
                self.output,
                to,
                output,
                &mut self.last,
            )?;
        }
        Ok(())
    }

    fn finish(
        &mut self,
        output: &mut format::context::Output,
        to: Rational,
    ) -> Result<(), ffmpeg_next::Error> {
        if let Some(mut source) = self.graph.get("in") {
            source.source().flush()?;
        }
        self.pull(output, to)?;
        self.encoder.send_eof()?;
        let from = self.encoder.time_base();
        write(
            &mut self.encoder,
            from,
            self.output,
            to,
            output,
            &mut self.last,
        )
    }
}
