#!/usr/bin/env bash
# Builds the program: first the ffmpeg it links, trimmed to codecs.conf and
# LGPL only, with the libraries it uses, from pinned sources; then the
# program against it, statically, as one .exe.
#
#   ./build.sh                  the free group alone
#   ./build.sh --with=h264,vc1  and those groups of codecs.conf
#   ./build.sh --test           and then its tests, against that build
#
# On Windows it runs in MSYS2's MINGW64 shell (or bash -l with MSYSTEM set
# to MINGW64), with the packages README.md names. What it builds of ffmpeg
# is kept under target/ffmpeg/, by what went into it, and made again only
# when that changes. It prints the path of the program it made last, beside
# which NOTICES.txt has the licences of what's built into it.

set -euo pipefail
cd "$(dirname "$0")"
ROOT=$PWD

# The sources, each a URL and the sha256 of what's there.
FFMPEG_VERSION=9.0.2
FFMPEG_URL=https://ffmpeg.org/releases/ffmpeg-$FFMPEG_VERSION.tar.xz
FFMPEG_SHA256=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
VPX_VERSION=1.17.0
VPX_URL=https://github.com/webmproject/libvpx/archive/refs/tags/v$VPX_VERSION.tar.gz
VPX_SHA256=1020f184046187baa2985dbde38e0691f49c44088bca7a1842b0236c6081dc0a
OPUS_VERSION=1.5.2
OPUS_URL=https://downloads.xiph.org/releases/opus/opus-$OPUS_VERSION.tar.gz
OPUS_SHA256=65c1d2f78b9f2fb20082c38cbe47c951ad5839345876e46941612ee87f9a7ce1

WITH=""
TEST=""
for argument in "$@"; do
	case $argument in
	--with=*) WITH=${argument#--with=} ;;
	--test) TEST=1 ;;
	*)
		echo "Usage: ./build.sh [--with=group,group] [--test]" >&2
		exit 2
		;;
	esac
done

case "${MSYSTEM:-}" in
MINGW64) TARGET=x86_64-pc-windows-gnu ;;
*)
	echo "Run it in MSYS2's MINGW64 shell." >&2
	exit 2
	;;
esac

# The groups asked for, each one codecs.conf has. (Not GROUPS, which is
# bash's own and can't be set.)
CHOSEN=(free)
IFS=, read -ra ASKED <<<"$WITH"
for group in "${ASKED[@]}"; do
	[ -z "$group" ] && continue
	if ! grep -q "^$group " codecs.conf; then
		echo "codecs.conf has no group $group" >&2
		exit 2
	fi
	CHOSEN+=("$group")
done

# What ffmpeg is enabled with, from the groups' lines.
ENABLE=()
while read -r group kind names; do
	case $group in "" | \#*) continue ;; esac
	[[ " ${CHOSEN[*]} " == *" $group "* ]] || continue
	for name in $names; do ENABLE+=("--enable-$kind=$name"); done
done <codecs.conf
if [ ${#ENABLE[@]} -eq 0 ]; then
	echo "codecs.conf enabled nothing" >&2
	exit 1
fi

# How each is configured, beside where it's installed.
OPUS_OPTIONS=(--enable-static --disable-shared --disable-doc --disable-extra-programs)
VPX_OPTIONS=(--target=x86_64-win64-gcc --enable-static --disable-shared
	--disable-examples --disable-tools --disable-docs --disable-unit-tests
	--enable-vp9 --disable-vp8)
FFMPEG_OPTIONS=(--enable-static --disable-shared --pkg-config-flags=--static
	--disable-programs --disable-doc --disable-network
	--disable-autodetect --disable-everything --disable-avdevice
	--enable-zlib --enable-libvpx --enable-libopus
	# Windows' own H.264 encoder, which ffmpeg drives through Direct3D 11.
	--enable-mediafoundation --enable-d3d11va
	"${ENABLE[@]}")

# Where each is built and installed, by what went into it and into those
# it's built on, so a change to one makes only it and what's on it again.
key() { printf '%s\n' "$@" | sha256sum | cut -c1-12; }
LIBS_KEY=$(key "$OPUS_SHA256" "${OPUS_OPTIONS[@]}" "$VPX_SHA256" "${VPX_OPTIONS[@]}")
LIBS=$ROOT/target/ffmpeg/$TARGET/libs-$LIBS_KEY
PREFIX=$ROOT/target/ffmpeg/$TARGET/ffmpeg-$(key "$LIBS_KEY" "$FFMPEG_SHA256" "${FFMPEG_OPTIONS[@]}")
SOURCES=$ROOT/target/sources
mkdir -p "$SOURCES"

# Downloads a source once and checks it, then unpacks it into a directory
# of its own under where it's built, anew.
fetch() {
	local url=$1 sha=$2 into=$3
	local file=$SOURCES/${url##*/}
	if [ ! -f "$file" ]; then curl -fsSL -o "$file.part" "$url" && mv "$file.part" "$file"; fi
	local got
	got=$(sha256sum "$file" | cut -d' ' -f1)
	if [ "$got" != "$sha" ]; then
		echo "$file is $got, not $sha" >&2
		exit 1
	fi
	rm -rf "$into"
	mkdir -p "$into"
	tar -xf "$file" -C "$into" --strip-components=1
}

JOBS=$(nproc)

if [ ! -f "$LIBS/.done" ]; then
	rm -rf "$LIBS"
	fetch "$OPUS_URL" "$OPUS_SHA256" "$LIBS/build/opus"
	(cd "$LIBS/build/opus" && ./configure --prefix="$LIBS" "${OPUS_OPTIONS[@]}" &&
		make -j"$JOBS" && make install)
	fetch "$VPX_URL" "$VPX_SHA256" "$LIBS/build/libvpx"
	(cd "$LIBS/build/libvpx" && ./configure --prefix="$LIBS" "${VPX_OPTIONS[@]}" &&
		make -j"$JOBS" && make install)
	touch "$LIBS/.done"
fi

if [ ! -f "$PREFIX/.done" ]; then
	rm -rf "$PREFIX"
	fetch "$FFMPEG_URL" "$FFMPEG_SHA256" "$PREFIX/build/ffmpeg"
	(cd "$PREFIX/build/ffmpeg" &&
		PKG_CONFIG_PATH="$LIBS/lib/pkgconfig" ./configure --prefix="$PREFIX" \
			"${FFMPEG_OPTIONS[@]}" &&
		make -j"$JOBS" && make install)

	# LGPL alone: nothing GPL or non-free got in by a library or an option.
	CONFIG=$PREFIX/build/ffmpeg/config.h
	if ! grep -q '^#define CONFIG_GPL 0' "$CONFIG" ||
		! grep -q '^#define CONFIG_NONFREE 0' "$CONFIG"; then
		echo "ffmpeg was configured with GPL or non-free parts" >&2
		exit 1
	fi
	touch "$PREFIX/.done"
fi

# The program, against that ffmpeg, which ffmpeg-sys-next finds by
# pkg-config, with the libraries it was linked with.
export PKG_CONFIG_PATH=$PREFIX/lib/pkgconfig:$LIBS/lib/pkgconfig
export PKG_CONFIG_ALL_STATIC=1
# Rust on Windows is mostly the MSVC one, to which this target is another
# machine's, where pkg-config isn't asked unless it's said to be.
export PKG_CONFIG_ALLOW_CROSS=1
# MinGW's compiler, not MSVC's, which the cc crate would otherwise take
# where Visual Studio is installed; and MinGW's libclang for the bindings.
export CC_x86_64_pc_windows_gnu=gcc AR_x86_64_pc_windows_gnu=ar
LIBCLANG_PATH=$(cygpath -m "$(dirname "$(command -v gcc)")")
export LIBCLANG_PATH
# Windows' headers, which ffmpeg's for Direct3D bring in, bring in clang's
# x86 intrinsics through MinGW's intrin.h, among them AMX's tile type, packed
# and aligned as Rust can't have it. ffmpeg's API has none of them, so their
# headers are taken as read.
export BINDGEN_EXTRA_CLANG_ARGS="-D__IMMINTRIN_H -D__X86INTRIN_H"
cargo build --release --locked --target "$TARGET"
# Where cargo puts what it builds, which a configuration may have elsewhere.
OUT=$(cargo metadata --format-version 1 --no-deps |
	sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | sed 's/\\\\/\//g')
RELEASE=$OUT/$TARGET/release

# The licences of what's built into the program, which go wherever it does:
# its own, ffmpeg's and the libraries' it links, from their sources.
section() {
	printf '%s\n\n' "------------------------------------------------------------------------"
	printf '%s\n\n' "$1"
	shift
	for file in "$@"; do tr -d '\r' <"$file" && printf '\n'; done
}
{
	section "ffmpeg-rs $(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1): LGPL 2.1 or later" LICENSE
	section "FFmpeg $FFMPEG_VERSION ($FFMPEG_URL): LGPL 2.1 or later" \
		"$PREFIX/build/ffmpeg/LICENSE.md" "$PREFIX/build/ffmpeg/COPYING.LGPLv2.1"
	section "libvpx $VPX_VERSION ($VPX_URL): BSD 3-clause, and its patent grant" \
		"$LIBS/build/libvpx/LICENSE" "$LIBS/build/libvpx/PATENTS"
	section "Opus $OPUS_VERSION ($OPUS_URL): BSD 3-clause" "$LIBS/build/opus/COPYING"
	section "zlib, as MSYS2's mingw-w64-x86_64-zlib has it: zlib License" \
		"$(dirname "$(command -v gcc)")/../share/licenses/zlib/LICENSE"
} >"$RELEASE/NOTICES.txt"

if [ -n "$TEST" ]; then
	# The tests ask the program which groups it was built with.
	FFMPEG_RS_GROUPS=$(
		IFS=,
		echo "${CHOSEN[*]}"
	) cargo test --release --locked --target "$TARGET"
fi
echo "$RELEASE/ffmpeg-rs.exe"
