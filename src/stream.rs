//! One video into a fragmented MP4 on stdout, which a webview plays as it
//! comes: each of its first picture and first sound copied as it is, or
//! made again, from a time on, until it ends or the program is killed.
//!
//!   ffmpeg-rs stream --input <path> [--start <ms>] [--video copy|transcode]
//!     [--audio copy|transcode|none] [--encoder auto|h264|vp9]
//!
//! A picture is made again as H.264 by Windows' own encoder where the
//! machine has one, and as VP9 where it hasn't, deinterlaced and with square
//! pixels; a sound as Opus. All of them play in an MP4. --encoder vp9 makes
//! it VP9 where there is a hardware encoder too, as a machine without one
//! would, and --encoder h264 fails where there's none.

use std::process::ExitCode;

use ffmpeg_next::{
    Dictionary, Packet, Rational, codec, decoder, encoder, filter, format, frame, media,
};

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
    start_ms: i64,
    video: Way,
    audio: Way,
    encoder: Encoder,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        input: String::new(),
        start_ms: 0,
        video: Way::Copy,
        audio: Way::Copy,
        encoder: Encoder::Auto,
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
            "--start" => options.start_ms = value.parse().map_err(|_| "--start is milliseconds")?,
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
    let result =
        options(args).and_then(|options| stream(&options).map_err(|error| error.to_string()));
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
    Copy { output: usize, time_base: Rational },
    Video(Box<VideoLane>),
    Audio(Box<AudioLane>),
}

struct VideoLane {
    output: usize,
    decoder: decoder::Video,
    graph: filter::Graph,
    encoder: encoder::Video,
}

struct AudioLane {
    output: usize,
    decoder: decoder::Audio,
    graph: filter::Graph,
    encoder: encoder::Audio,
}

fn stream(options: &Options) -> Result<(), ffmpeg_next::Error> {
    let mut input = format::input(&options.input)?;
    let video = input
        .streams()
        .best(media::Type::Video)
        .map(|stream| stream.index());
    let audio = input
        .streams()
        .best(media::Type::Audio)
        .map(|stream| stream.index());

    // Fragments from the first keyframe on, with no index at the end to
    // wait for, and times from zero wherever it starts.
    let mut muxing = Dictionary::new();
    muxing.set("movflags", "frag_keyframe+empty_moov+default_base_moof");
    muxing.set("avoid_negative_ts", "make_zero");
    let mut output = format::output_as("pipe:1", "mp4")?;

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
                }
            }
            (Way::Transcode, media::Type::Video) => {
                Lane::Video(Box::new(video_lane(&mut output, &stream, options.encoder)?))
            }
            (Way::Transcode, media::Type::Audio) => {
                Lane::Audio(Box::new(audio_lane(&mut output, &stream)?))
            }
            (Way::Transcode, _) => Lane::Skip,
        };
        lanes.push(lane);
    }

    if options.start_ms > 0 {
        let target = options.start_ms * i64::from(ffmpeg_next::ffi::AV_TIME_BASE) / 1000;
        input.seek(target, ..target)?;
    }
    output.write_header_with(muxing)?;
    let time_bases: Vec<Rational> = (0..output.nb_streams() as usize)
        .map(|index| {
            output
                .stream(index)
                .map(|stream| stream.time_base())
                .unwrap_or(Rational(1, 1))
        })
        .collect();

    for (stream, mut packet) in input.packets() {
        let Some(lane) = lanes.get_mut(stream.index()) else {
            continue;
        };
        match lane {
            Lane::Skip => {}
            Lane::Copy {
                output: index,
                time_base,
            } => {
                packet.rescale_ts(*time_base, time_bases[*index]);
                packet.set_position(-1);
                packet.set_stream(*index);
                packet.write_interleaved(&mut output)?;
            }
            Lane::Video(lane) => {
                // A packet that can't be decoded is passed over, as a player
                // would.
                if lane.decoder.send_packet(&packet).is_ok() {
                    lane.drain(&mut output, time_bases[lane.output])?;
                }
            }
            Lane::Audio(lane) => {
                if lane.decoder.send_packet(&packet).is_ok() {
                    lane.drain(&mut output, time_bases[lane.output])?;
                }
            }
        }
    }
    for lane in &mut lanes {
        match lane {
            Lane::Video(lane) => {
                let _ = lane.decoder.send_eof();
                lane.drain(&mut output, time_bases[lane.output])?;
                lane.finish(&mut output, time_bases[lane.output])?;
            }
            Lane::Audio(lane) => {
                let _ = lane.decoder.send_eof();
                lane.drain(&mut output, time_bases[lane.output])?;
                lane.finish(&mut output, time_bases[lane.output])?;
            }
            _ => {}
        }
    }
    output.write_trailer()
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

fn video_lane(
    output: &mut format::context::Output,
    stream: &format::stream::Stream,
    choice: Encoder,
) -> Result<VideoLane, ffmpeg_next::Error> {
    let decoder = codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .video()?;
    let rate = match stream.avg_frame_rate() {
        rate if rate.numerator() > 0 => rate,
        _ => Rational(25, 1),
    };
    let aspect = match decoder.aspect_ratio() {
        aspect if aspect.numerator() > 0 => aspect,
        _ => Rational(1, 1),
    };
    let time_base = stream.time_base();
    let global = output
        .format()
        .flags()
        .contains(format::Flags::GLOBAL_HEADER);

    // Windows' hardware H.264 where there's one; VP9 where not.
    let hardware = match choice {
        Encoder::Vp9 => None,
        Encoder::Auto | Encoder::H264 => encoder::find_by_name("h264_mf"),
    };
    let (codec, pixels, settings) = match hardware {
        Some(codec) => {
            let mut settings = Dictionary::new();
            settings.set("hw_encoding", "1");
            settings.set("rate_control", "quality");
            (codec, format::Pixel::NV12, settings)
        }
        None if choice == Encoder::H264 => return Err(ffmpeg_next::Error::EncoderNotFound),
        None => (vp9()?, format::Pixel::YUV420P, vp9_settings()),
    };
    let spec = |pixels: format::Pixel| {
        format!(
            // A frame for each frame, not one for each field.
            "bwdif=mode=send_frame:deint=interlaced,scale='trunc(iw*sar/2)*2':ih,setsar=1,format={}",
            pixels
                .descriptor()
                .map(|descriptor| descriptor.name())
                .unwrap_or("yuv420p")
        )
    };
    let source = format!(
        "video_size={}x{}:pix_fmt={}:time_base={}/{}:pixel_aspect={}/{}",
        decoder.width(),
        decoder.height(),
        ffmpeg_next::ffi::AVPixelFormat::from(decoder.format()) as i32,
        time_base.numerator(),
        time_base.denominator(),
        aspect.numerator(),
        aspect.denominator(),
    );
    let width = (decoder.width() as f64 * f64::from(aspect)).round() as u32 / 2 * 2;
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
        context.set_height(decoder.height());
        context.set_format(pixels);
        context.set_frame_rate(Some(rate));
        context.set_time_base(shown);
        if global {
            context.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        Ok::<_, ffmpeg_next::Error>((graph, context.open_as_with(codec, settings)?))
    };
    // A machine without the hardware has the encoder but can't open it.
    let (graph, encoder) = match open(codec, pixels, settings) {
        Ok(opened) => opened,
        Err(error) if choice == Encoder::H264 => return Err(error),
        Err(_) => open(vp9()?, format::Pixel::YUV420P, vp9_settings())?,
    };
    let mut added = output.add_stream(encoder.codec())?;
    added.set_parameters(&encoder);
    added.set_time_base(encoder.time_base());
    Ok(VideoLane {
        output: added.index(),
        graph,
        decoder,
        encoder,
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

fn audio_lane(
    output: &mut format::context::Output,
    stream: &format::stream::Stream,
) -> Result<AudioLane, ffmpeg_next::Error> {
    let decoder = codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .audio()?;
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

    let time_base = stream.time_base();
    let source = format!(
        "time_base={}/{}:sample_rate={}:sample_fmt={}:channel_layout={}",
        time_base.numerator(),
        time_base.denominator(),
        decoder.rate(),
        decoder.format().name(),
        layout_mask(&decoder),
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

// Writes what an encoder has made.
fn write(
    encoder: &mut encoder::encoder::Encoder,
    from: Rational,
    index: usize,
    to: Rational,
    output: &mut format::context::Output,
) -> Result<(), ffmpeg_next::Error> {
    let mut packet = Packet::empty();
    while encoder.receive_packet(&mut packet).is_ok() {
        packet.set_stream(index);
        packet.rescale_ts(from, to);
        packet.write_interleaved(output)?;
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
        let mut decoded = frame::Video::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let timestamp = decoded.timestamp();
            decoded.set_pts(timestamp);
            if let Some(mut source) = self.graph.get("in") {
                source.source().add(&decoded)?;
            }
            self.pull(output, to)?;
        }
        Ok(())
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
            write(&mut self.encoder, from, self.output, to, output)?;
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
        write(&mut self.encoder, from, self.output, to, output)
    }
}

impl AudioLane {
    fn drain(
        &mut self,
        output: &mut format::context::Output,
        to: Rational,
    ) -> Result<(), ffmpeg_next::Error> {
        let mut decoded = frame::Audio::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let timestamp = decoded.timestamp();
            decoded.set_pts(timestamp);
            if let Some(mut source) = self.graph.get("in") {
                source.source().add(&decoded)?;
            }
            self.pull(output, to)?;
        }
        Ok(())
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
            write(&mut self.encoder, from, self.output, to, output)?;
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
        write(&mut self.encoder, from, self.output, to, output)
    }
}
