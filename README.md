# ffmpeg-rs

A small program over a trimmed ffmpeg, built LGPL alone, for an app to start as a process of its own. It does four things:

- **caps**: what this build decodes, encodes, reads and writes, from ffmpeg's own registry.
- **frame**: a frame of a video as a PNG, from some way through it, deinterlaced where it's interlaced, with square pixels, scaled down to fit.
- **stream**: a video into a fragmented MP4 on stdout that a webview plays as it comes, each of its picture and sound copied as it is or made again: the picture as H.264 by Windows' own encoder where the machine has one and as VP9 where it hasn't, the sound as Opus.
- **render**: the same into an MP4 file to keep, its index first: the picture fitted inside a largest size and made at a quality, by the machine's hardware H.264, then Windows' software H.264, then VP9, and saying how far it is as it goes.

It's written for a desktop photo and video library, to play and show the videos a webview can't: camcorders' DV and AVCHD, DVDs, Xvid and DivX, old phones' 3GP, Flash, Windows Media and the like.

## Use

```
ffmpeg-rs serve       stays open: JSON lines in, JSON lines out
ffmpeg-rs stream --input <path> [--start <ms>] [--video copy|transcode] [--audio copy|transcode|none] [--encoder auto|h264|vp9]
ffmpeg-rs render --input <path> --output <path> [--max-size <pixels>] [--quality 1-100] [--video copy|transcode] [--audio copy|transcode|none] [--encoder auto|h264|vp9]
ffmpeg-rs --protocol  the JSON Schema of serve's requests and replies (protocol.json)
ffmpeg-rs --version   this program's version, ffmpeg's, and the line ffmpeg was configured with
```

What the build has no decoder for, as H.264, HEVC, VC-1 and AAC, `stream` and `render` decode by the system's own decoders where they make it again: on Windows Media Foundation's source reader, which reads the file, decodes its first picture or first sound, deinterlaces the picture and turns it upright. What the build does decode beside it, as a camcorder's AC-3 beside its H.264, is the build's, copied or made again as asked, so a Windows without a decoder for that sound misses nothing; a file the build can't read at all is the system's alone. `caps` says which of those the machine has as `system`. Elsewhere there's none yet. A picture ffmpeg decodes is turned upright by its display matrix, as a phone's is, by ffmpeg's own filters.

`render` writes `{"at":<ms>}` on stdout as each second of the video is made, from its start. Its quality is 75 where it isn't said; it's Media Foundation's for H.264 and a constant quality for VP9 (75 is crf 27). A file it leaves after it failed or was killed is half made, so its caller writes it under a name of its own and renames it once it's done.

On a 10-second 1080p clip at 30 frames with its sound, on an i7-4790K: H.264 by the machine's encoder in 2.5 s and VP9 in 3.2 s; Windows' software H.264 took 1.6 s by ffmpeg's own command, and libvpx's good setting 13.2 s for a file 13 % smaller.

`serve` takes `{"op":"caps","id":1}`, `{"op":"frame","id":2,"path":"…","at":0.1,"size":512}` and `{"op":"stop"}`, and answers each but `stop` with `{"id":…,"ok":…}` or `{"id":…,"error":{"code":…,"message":…}}`, in the order they came; a frame's answer is followed by its PNG, of as many bytes as it says.

## What it's built with

[codecs.conf](codecs.conf) lists what ffmpeg is built with, in groups. The group `free` is always built: what has no patent left on it, is free of royalties, or only drives the system's own codec. The other groups, H.264, HEVC and VC-1 decoding and AAC encoding, are codecs still under patent, built only when asked for:

```
./build.sh                  the free group alone
./build.sh --with=h264,vc1  and those groups
```

`--version` and `caps` say what a build has, so a program built either way can be told apart.

ffmpeg is built static with `--disable-everything` and only what the groups name, with libvpx and Opus, and `build.sh` refuses a build that `config.h` says has anything GPL or non-free in it.

## Building

On Windows, in [MSYS2](https://www.msys2.org)'s MINGW64 shell, with:

```
pacman -S --needed make diffutils git mingw-w64-x86_64-toolchain mingw-w64-x86_64-nasm \
  mingw-w64-x86_64-pkgconf mingw-w64-x86_64-clang mingw-w64-x86_64-zlib
rustup target add x86_64-pc-windows-gnu
```

and Rust on the PATH. `./build.sh` downloads ffmpeg, libvpx and Opus at the versions it pins, each checked by its sha256, builds them into `target/ffmpeg/`, by what went into them, so they're built again only when that changes, and then the program against them, and prints the path of the `.exe`, beside which `NOTICES.txt` has the licences of everything built into it. The first build takes some minutes; the ones after, while nothing in ffmpeg changes, as long as cargo does. `./build.sh --test` runs the tests against the build, on the clips in `tests/fixtures`.

CI builds and tests it with the free group and with every group. A tag `v*` releases the free build alone, with its notices and their sha256: no build with a codec under patent is given out. The tag builds nothing itself: it releases the build of the same commit's run on `main`, waiting for it if it's still going, so `main` is pushed before the tag, and a commit whose run failed isn't released.

## License

LGPL 2.1 or later, as ffmpeg's own LGPL parts are; see [LICENSE](LICENSE). A build of it contains ffmpeg (LGPL 2.1 or later), libvpx (BSD 3-clause), Opus (BSD 3-clause) and zlib (zlib), whose sources are at the versions `build.sh` names.
