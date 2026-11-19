# ffmpeg-rs

A small program over a trimmed ffmpeg, built LGPL alone, for an app to start as a process of its own. It does three things:

- **caps**: what this build decodes, encodes, reads and writes, from ffmpeg's own registry.
- **frame**: a frame of a video as a PNG, from some way through it, deinterlaced where it's interlaced, with square pixels, scaled down to fit.
- **stream**: a video into a fragmented MP4 on stdout that a webview plays as it comes, each of its picture and sound copied as it is or made again: the picture as H.264 by Windows' own encoder where the machine has one and as VP9 where it hasn't, the sound as Opus.

It's written for a desktop photo and video library, to play and show the videos a webview can't: camcorders' DV and AVCHD, DVDs, Xvid and DivX, old phones' 3GP, Flash, Windows Media and the like.

## Use

```
ffmpeg-rs serve       stays open: JSON lines in, JSON lines out
ffmpeg-rs stream --input <path> [--start <ms>] [--video copy|transcode] [--audio copy|transcode|none] [--encoder auto|h264|vp9]
ffmpeg-rs --protocol  the JSON Schema of serve's requests and replies (protocol.json)
ffmpeg-rs --version   this program's version, ffmpeg's, and the line ffmpeg was configured with
```

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

and Rust on the PATH. `./build.sh` downloads ffmpeg, libvpx and Opus at the versions it pins, each checked by its sha256, builds them into `target/ffmpeg/`, by what went into them, so they're built again only when that changes, and then the program against them, and prints the path of the `.exe`. The first build takes some minutes; the ones after, while nothing in ffmpeg changes, as long as cargo does.

## License

LGPL 2.1 or later, as ffmpeg's own LGPL parts are; see [LICENSE](LICENSE). A build of it contains ffmpeg (LGPL 2.1 or later), libvpx (BSD 3-clause), Opus (BSD 3-clause) and zlib (zlib), whose sources are at the versions `build.sh` names.
