//! A frame of a video as a PNG: from some way through it, deinterlaced where
//! it's interlaced, its pixels made square, and scaled down to fit.

use ffmpeg_next::{codec, decoder, encoder, filter, format, frame, media, util};

use crate::protocol::Failure;

pub struct Png {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

// The filters a frame goes through: bwdif only touches a frame marked
// interlaced; DV's and DVDs' pixels aren't square, so it's widened by their
// aspect; then made to fit `size`, and RGB for the PNG.
fn filters(size: u32) -> String {
    format!(
        "bwdif=mode=send_frame:deint=interlaced,scale='trunc(iw*sar/2)*2':ih,setsar=1,\
		scale=w='min(iw,{size})':h='min(ih,{size})':force_original_aspect_ratio=decrease,\
		format=rgb24"
    )
}

pub fn frame(path: &str, at: f64, size: u32) -> Result<Png, Failure> {
    let mut input = format::input(&path).map_err(|error| Failure::new("open", error))?;
    let stream = input
        .streams()
        .best(media::Type::Video)
        .ok_or_else(|| Failure::new("no-video", "the file has no picture"))?;
    let index = stream.index();
    let time_base = stream.time_base();
    let context = codec::context::Context::from_parameters(stream.parameters())
        .map_err(|error| Failure::new("decoder", error))?;
    let mut decoder = context
        .decoder()
        .video()
        .map_err(|error| Failure::new("decoder", error))?;

    // Some way through, by the container's duration, from the keyframe
    // before it; a file that doesn't say is taken from its start.
    let duration = input.duration();
    if duration > 0 {
        let target = (duration as f64 * at.clamp(0.0, 1.0)) as i64;
        if input.seek(target, ..target).is_err() {
            let _ = input.seek(0, ..0);
        }
    }
    let target = if duration > 0 {
        let seconds =
            duration as f64 * at.clamp(0.0, 1.0) / f64::from(ffmpeg_next::ffi::AV_TIME_BASE);
        Some((seconds / f64::from(time_base)) as i64)
    } else {
        None
    };

    let mut decoded = frame::Video::empty();
    let mut chosen: Option<frame::Video> = None;
    let mut take = |decoder: &mut decoder::Video, chosen: &mut Option<frame::Video>| {
        while decoder.receive_frame(&mut decoded).is_ok() {
            // The first frame at or after the time, or the last before it
            // where none is.
            let reached = match (target, decoded.timestamp()) {
                (Some(target), Some(at)) => at >= target,
                _ => true,
            };
            *chosen = Some(decoded.clone());
            if reached {
                return true;
            }
        }
        false
    };
    let mut done = false;
    for (packet_stream, packet) in input.packets() {
        if packet_stream.index() != index {
            continue;
        }
        if decoder.send_packet(&packet).is_err() {
            continue;
        }
        if take(&mut decoder, &mut chosen) {
            done = true;
            break;
        }
    }
    if !done {
        let _ = decoder.send_eof();
        take(&mut decoder, &mut chosen);
    }
    let picture = chosen.ok_or_else(|| Failure::new("decode", "no frame could be decoded"))?;
    let shown = filtered(&decoder, time_base, &picture, size)?;
    encode(&shown)
}

fn filtered(
    decoder: &decoder::Video,
    time_base: util::rational::Rational,
    picture: &frame::Video,
    size: u32,
) -> Result<frame::Video, Failure> {
    let fail = |error: ffmpeg_next::Error| Failure::new("filter", error);
    let aspect = decoder.aspect_ratio();
    let aspect = if aspect.numerator() > 0 {
        aspect
    } else {
        util::rational::Rational(1, 1)
    };
    let mut graph = filter::Graph::new();
    let source = format!(
        "video_size={}x{}:pix_fmt={}:time_base={}/{}:pixel_aspect={}/{}",
        picture.width(),
        picture.height(),
        ffmpeg_next::ffi::AVPixelFormat::from(picture.format()) as i32,
        time_base.numerator(),
        time_base.denominator(),
        aspect.numerator(),
        aspect.denominator(),
    );
    graph
        .add(
            &filter::find("buffer").ok_or_else(|| fail(ffmpeg_next::Error::FilterNotFound))?,
            "in",
            &source,
        )
        .map_err(fail)?;
    graph
        .add(
            &filter::find("buffersink").ok_or_else(|| fail(ffmpeg_next::Error::FilterNotFound))?,
            "out",
            "",
        )
        .map_err(fail)?;
    graph
        .output("in", 0)
        .and_then(|parser| parser.input("out", 0))
        .and_then(|parser| parser.parse(&filters(size)))
        .map_err(fail)?;
    graph.validate().map_err(fail)?;
    graph
        .get("in")
        .ok_or_else(|| fail(ffmpeg_next::Error::FilterNotFound))?
        .source()
        .add(picture)
        .map_err(fail)?;
    graph
        .get("in")
        .ok_or_else(|| fail(ffmpeg_next::Error::FilterNotFound))?
        .source()
        .flush()
        .map_err(fail)?;
    let mut shown = frame::Video::empty();
    graph
        .get("out")
        .ok_or_else(|| fail(ffmpeg_next::Error::FilterNotFound))?
        .sink()
        .frame(&mut shown)
        .map_err(fail)?;
    Ok(shown)
}

fn encode(shown: &frame::Video) -> Result<Png, Failure> {
    let fail = |error: ffmpeg_next::Error| Failure::new("encode", error);
    let codec =
        encoder::find(codec::Id::PNG).ok_or_else(|| fail(ffmpeg_next::Error::EncoderNotFound))?;
    let mut context = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(fail)?;
    context.set_width(shown.width());
    context.set_height(shown.height());
    context.set_format(shown.format());
    context.set_time_base((1, 25));
    let mut png = context.open_as(codec).map_err(fail)?;
    let mut picture = shown.clone();
    picture.set_pts(Some(0));
    png.send_frame(&picture).map_err(fail)?;
    png.send_eof().map_err(fail)?;
    let mut packet = codec::packet::Packet::empty();
    png.receive_packet(&mut packet).map_err(fail)?;
    Ok(Png {
        width: shown.width(),
        height: shown.height(),
        data: packet.data().map(<[u8]>::to_vec).unwrap_or_default(),
    })
}
