//! The system's own decoders, for what this build has none for: H.264, HEVC
//! and VC-1, and AAC, which a phone's or a camera's video has, where the
//! platform has them. On Windows that's Media Foundation's source reader,
//! which reads the file and decodes its first picture and first sound into
//! frames: a picture as NV12, deinterlaced by its video processor, and a
//! sound as 32-bit floats. Its times are in 100 ns from the file's start.
//! Elsewhere there's none yet, and opening fails.

use ffmpeg_next::{Rational, frame};

/// The time base of what the reader gives.
pub const TIME_BASE: Rational = Rational(1, 10_000_000);

/// A picture as the reader gives it: its size in pixels, their shape, its
/// frame rate where it says one, and how far it's to be turned clockwise to
/// be upright, in degrees.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub aspect: Rational,
    pub rate: Option<Rational>,
    pub turn: u32,
}

/// A sound as the reader gives it.
pub struct Sound {
    pub rate: u32,
    pub channels: u16,
}

pub enum Decoded {
    Video(frame::Video),
    Audio(frame::Audio),
}

/// The subtypes of what the system decodes, as caps names them.
pub fn decoders() -> Vec<String> {
    imp::decoders()
}

pub use imp::Reader;

#[cfg(windows)]
mod imp {
    use std::sync::Once;

    use ffmpeg_next::{ChannelLayout, Rational, format, frame};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
    use windows::core::{GUID, HSTRING};

    use super::{Decoded, Picture, Sound};

    static START: Once = Once::new();

    // COM on this thread, and Media Foundation once for the process.
    fn start() {
        // SAFETY: COM is started for the calling thread, which is the one
        // that uses the reader; a second call on it is harmless.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        START.call_once(|| {
            // SAFETY: MFStartup with the version it was built against; it's
            // left started for as long as the process runs.
            let _ = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) };
        });
    }

    fn high_low(value: u64) -> (u32, u32) {
        ((value >> 32) as u32, value as u32)
    }

    pub struct Reader {
        reader: IMFSourceReader,
        pub picture: Option<Picture>,
        pub sound: Option<Sound>,
        stride: usize,
        // Whether each of the picture and the sound is still to end.
        reading: [bool; 2],
    }

    const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    const AUDIO: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;

    impl Reader {
        /// Opens the file for its picture, its sound or both, decoded by
        /// the system. Fails where the system can't read the file, or can't
        /// decode what's asked of it.
        pub fn open(path: &str, video: bool, audio: bool) -> Result<Reader, String> {
            start();
            // SAFETY: each call is Media Foundation's, on objects it made.
            unsafe {
                let mut attributes = None;
                MFCreateAttributes(&mut attributes, 1).map_err(|error| error.to_string())?;
                let attributes = attributes.ok_or("no attributes")?;
                attributes
                    .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
                    .map_err(|error| error.to_string())?;
                let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path), &attributes)
                    .map_err(|error| format!("the system can't read it: {error}"))?;
                let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
                let mut opened = Reader {
                    reader,
                    picture: None,
                    sound: None,
                    stride: 0,
                    reading: [false, false],
                };
                if video {
                    opened.open_video()?;
                }
                if audio {
                    opened.open_audio()?;
                }
                Ok(opened)
            }
        }

        unsafe fn open_video(&mut self) -> Result<(), String> {
            let reader = &self.reader;
            // SAFETY: as open's.
            unsafe {
                if reader.GetNativeMediaType(VIDEO, 0).is_err() {
                    return Ok(());
                }
                let wanted = MFCreateMediaType().map_err(|error| error.to_string())?;
                wanted
                    .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                    .and_then(|()| wanted.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12))
                    .and_then(|()| {
                        wanted
                            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                    })
                    .map_err(|error| error.to_string())?;
                reader
                    .SetStreamSelection(VIDEO, true)
                    .map_err(|error| error.to_string())?;
                reader
                    .SetCurrentMediaType(VIDEO, None, &wanted)
                    .map_err(|error| {
                        format!("the system has no decoder for its picture: {error}")
                    })?;
                let current = reader
                    .GetCurrentMediaType(VIDEO)
                    .map_err(|error| error.to_string())?;
                let (width, height) = high_low(
                    current
                        .GetUINT64(&MF_MT_FRAME_SIZE)
                        .map_err(|error| error.to_string())?,
                );
                let aspect = match current.GetUINT64(&MF_MT_PIXEL_ASPECT_RATIO).map(high_low) {
                    Ok((num, den)) if num > 0 && den > 0 => Rational(num as i32, den as i32),
                    _ => Rational(1, 1),
                };
                let rate = match current.GetUINT64(&MF_MT_FRAME_RATE).map(high_low) {
                    Ok((num, den)) if num > 0 && den > 0 => Some(Rational(num as i32, den as i32)),
                    _ => None,
                };
                // A stride that's said is as the buffer has it, which may be
                // more than the width; one that's negative is bottom up,
                // which NV12 never is.
                let stride = current
                    .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                    .map(|stride| (stride as i32).unsigned_abs() as usize)
                    .unwrap_or(width as usize);
                self.stride = stride.max(width as usize);
                self.picture = Some(Picture {
                    width,
                    height,
                    aspect,
                    rate,
                    // The video processor turns it by its rotation
                    // (MF_MT_VIDEO_ROTATION) itself, and its frame size is
                    // the size turned: what it gives is upright.
                    turn: 0,
                });
                self.reading[0] = true;
            }
            Ok(())
        }

        unsafe fn open_audio(&mut self) -> Result<(), String> {
            let reader = &self.reader;
            // SAFETY: as open's.
            unsafe {
                if reader.GetNativeMediaType(AUDIO, 0).is_err() {
                    return Ok(());
                }
                let wanted = MFCreateMediaType().map_err(|error| error.to_string())?;
                wanted
                    .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                    .and_then(|()| wanted.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float))
                    .map_err(|error| error.to_string())?;
                reader
                    .SetStreamSelection(AUDIO, true)
                    .map_err(|error| error.to_string())?;
                reader
                    .SetCurrentMediaType(AUDIO, None, &wanted)
                    .map_err(|error| format!("the system has no decoder for its sound: {error}"))?;
                let current = reader
                    .GetCurrentMediaType(AUDIO)
                    .map_err(|error| error.to_string())?;
                self.sound = Some(Sound {
                    rate: current
                        .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
                        .map_err(|error| error.to_string())?,
                    channels: current
                        .GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
                        .map_err(|error| error.to_string())? as u16,
                });
                self.reading[1] = true;
            }
            Ok(())
        }

        /// Goes to the keyframe at or before a time in milliseconds.
        pub fn seek(&self, ms: i64) -> Result<(), String> {
            let at = PROPVARIANT::from(ms * 10_000);
            // SAFETY: a time in 100 ns, in the format GUID_NULL names.
            unsafe { self.reader.SetCurrentPosition(&GUID::zeroed(), &at) }
                .map_err(|error| error.to_string())
        }

        /// The next frame of either, in the order the file has them, or
        /// None once both have ended.
        pub fn read(&mut self) -> Result<Option<Decoded>, String> {
            while self.reading.contains(&true) {
                let mut index = 0;
                let mut flags = 0;
                let mut time = 0;
                let mut sample = None;
                // SAFETY: the reader is synchronous, and fills what it's given.
                unsafe {
                    self.reader.ReadSample(
                        MF_SOURCE_READER_ANY_STREAM.0 as u32,
                        0,
                        Some(&mut index),
                        Some(&mut flags),
                        Some(&mut time),
                        Some(&mut sample),
                    )
                }
                .map_err(|error| error.to_string())?;
                let video = self.is_video(index);
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    self.reading[if video { 0 } else { 1 }] = false;
                }
                let Some(sample) = sample else {
                    continue;
                };
                let frame = if video {
                    Decoded::Video(self.picture_of(&sample, time)?)
                } else {
                    Decoded::Audio(self.sound_of(&sample, time)?)
                };
                return Ok(Some(frame));
            }
            Ok(None)
        }

        // Whether the stream a sample came from is the picture: the reader
        // says the stream by its index in the file.
        fn is_video(&self, index: u32) -> bool {
            // SAFETY: a question of the reader about a stream it has.
            unsafe { self.reader.GetCurrentMediaType(index) }
                .and_then(|kind| unsafe { kind.GetGUID(&MF_MT_MAJOR_TYPE) })
                .is_ok_and(|major| major == MFMediaType_Video)
        }

        fn picture_of(&self, sample: &IMFSample, time: i64) -> Result<frame::Video, String> {
            let picture = self.picture.as_ref().ok_or("no picture")?;
            let (width, height) = (picture.width as usize, picture.height as usize);
            let mut frame = frame::Video::new(format::Pixel::NV12, picture.width, picture.height);
            with_bytes(sample, |bytes| {
                // The rows of the luma, then of the chroma, which may start
                // below a height the decoder aligned.
                let rows = bytes.len() / self.stride * 2 / 3;
                let chroma = self.stride * rows.max(height);
                for (plane, from, count) in [(0, 0, height), (1, chroma, height.div_ceil(2))] {
                    let to_stride = frame.stride(plane);
                    let data = frame.data_mut(plane);
                    for row in 0..count {
                        let source = from + row * self.stride;
                        let Some(line) = bytes.get(source..source + width) else {
                            return Err("a picture smaller than it says".to_string());
                        };
                        data[row * to_stride..row * to_stride + width].copy_from_slice(line);
                    }
                }
                Ok(())
            })?;
            frame.set_pts(Some(time));
            Ok(frame)
        }

        fn sound_of(&self, sample: &IMFSample, time: i64) -> Result<frame::Audio, String> {
            let sound = self.sound.as_ref().ok_or("no sound")?;
            let channels = usize::from(sound.channels.max(1));
            let layout = ChannelLayout::default(i32::from(sound.channels.max(1)));
            with_bytes(sample, |bytes| {
                let samples = bytes.len() / 4 / channels;
                let mut frame = frame::Audio::new(
                    format::Sample::F32(format::sample::Type::Packed),
                    samples,
                    layout,
                );
                frame.set_rate(sound.rate);
                frame.data_mut(0)[..samples * 4 * channels]
                    .copy_from_slice(&bytes[..samples * 4 * channels]);
                frame.set_pts(Some(time));
                Ok(frame)
            })
        }
    }

    // Runs `use` on a sample's bytes, all in one buffer.
    fn with_bytes<T>(
        sample: &IMFSample,
        use_bytes: impl FnOnce(&[u8]) -> Result<T, String>,
    ) -> Result<T, String> {
        // SAFETY: the buffer is locked while its bytes are read, and
        // unlocked after; the slice doesn't outlive the lock.
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|error| error.to_string())?;
            let mut data = std::ptr::null_mut();
            let mut length = 0;
            buffer
                .Lock(&mut data, None, Some(&mut length))
                .map_err(|error| error.to_string())?;
            let result = use_bytes(std::slice::from_raw_parts(data, length as usize));
            let _ = buffer.Unlock();
            result
        }
    }

    // What the system has a decoder for, of what this build may not: each
    // subtype asked of Media Foundation's registry.
    pub fn decoders() -> Vec<String> {
        start();
        let asked = [
            (
                MFT_CATEGORY_VIDEO_DECODER,
                MFMediaType_Video,
                MFVideoFormat_H264,
                "h264",
            ),
            (
                MFT_CATEGORY_VIDEO_DECODER,
                MFMediaType_Video,
                MFVideoFormat_HEVC,
                "hevc",
            ),
            (
                MFT_CATEGORY_VIDEO_DECODER,
                MFMediaType_Video,
                MFVideoFormat_WVC1,
                "vc1",
            ),
            (
                MFT_CATEGORY_VIDEO_DECODER,
                MFMediaType_Video,
                MFVideoFormat_WMV3,
                "wmv3",
            ),
            (
                MFT_CATEGORY_AUDIO_DECODER,
                MFMediaType_Audio,
                MFAudioFormat_AAC,
                "aac",
            ),
        ];
        asked
            .into_iter()
            .filter(|(category, major, subtype, _)| {
                let input = MFT_REGISTER_TYPE_INFO {
                    guidMajorType: *major,
                    guidSubtype: *subtype,
                };
                let mut found: *mut Option<IMFActivate> = std::ptr::null_mut();
                let mut count = 0;
                // SAFETY: MFTEnumEx fills an array it allocated, of count
                // activates, which are released and the array freed.
                unsafe {
                    if MFTEnumEx(
                        *category,
                        MFT_ENUM_FLAG_ALL,
                        Some(&input),
                        None,
                        &mut found,
                        &mut count,
                    )
                    .is_err()
                    {
                        return false;
                    }
                    for index in 0..count as usize {
                        drop(found.add(index).read());
                    }
                    CoTaskMemFree(Some(found as *const _));
                }
                count > 0
            })
            .map(|(_, _, _, name)| name.to_string())
            .collect()
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{Decoded, Picture, Sound};

    pub struct Reader {
        pub picture: Option<Picture>,
        pub sound: Option<Sound>,
    }

    impl Reader {
        pub fn open(_: &str, _: bool, _: bool) -> Result<Reader, String> {
            Err("there's no system decoder on this platform yet".into())
        }
        pub fn seek(&self, _: i64) -> Result<(), String> {
            Ok(())
        }
        pub fn read(&mut self) -> Result<Option<Decoded>, String> {
            Ok(None)
        }
    }

    pub fn decoders() -> Vec<String> {
        Vec::new()
    }
}
