#!/bin/sh
# Regenerates the spumux-authored CVD and SVCD OGT fixtures in this
# directory. Third-party encoder output, not archived disc rips.
#
# Encoder: dvdauthor 0.7.2 spumux (GPL-2.0-or-later; only its output is
# kept here), from https://downloads.sourceforge.net/project/dvdauthor/dvdauthor-0.7.2.tar.gz
# sha256 3020a92de9f78eb36f48b6f22d5a001c47107826634a785a62dfcd080f612eb7
# (Homebrew's dvdauthor formula pins the same tarball). Built on macOS
# without ImageMagick, like Homebrew, with a freetype-config that prints
# Homebrew FreeType's flags:
#   PATH=<dir with that freetype-config>:/usr/bin:/bin:/usr/sbin:/sbin \
#   PKG_CONFIG=/opt/homebrew/bin/pkg-config PKG_CONFIG_LIBDIR=/nonexistent \
#   LIBPNG_CFLAGS=-I/opt/homebrew/opt/libpng/include/libpng16 \
#   LIBPNG_LIBS="-L/opt/homebrew/opt/libpng/lib -lpng16 -lz" \
#   ./configure --disable-dvdunauthor && make -C src spumux
# Video: ffmpeg 9.0.2. Usage: SPUMUX=/path/to/spumux ./generate.sh
# Committed outputs (sha256): svcd-spumux.mpg 1532a070c81ecff8b579a03db04b340de4d3b8b9fa8e6ae262cad145f7531d43,
# cvd-spumux.mpg edacc9ed15246eb24fa814e5a0a9463a327b5a9611fb6dd850b5a4a135e6b4c3. In both, the first
# subtitle (6632- and 4636-byte units) spans three 2324-byte private-stream packets.
set -eu
cd "$(dirname "$0")"
python3 make_pngs.py
for kind in svcd cvd; do
    size=480x480
    [ "$kind" = cvd ] && size=352x480
    ffmpeg -nostdin -v error -f lavfi -i "color=c=0x204060:s=$size:r=30000/1001:d=6" -an \
        -target ntsc-svcd -s "$size" -b:v 64k -maxrate 128k -bufsize 224k -y "base-$kind.mpg"
    "$SPUMUX" -m "$kind" -s 0 "$kind.xml" < "base-$kind.mpg" > "$kind-spumux.mpg"
    rm "base-$kind.mpg"
done
shasum -a 256 ./*.png ./*.xml ./*-spumux.mpg
