#!/usr/bin/env python3
"""Fetch fixed test fonts outside the source tree; nothing is bundled in the player."""
import argparse
import hashlib
import io
from pathlib import Path
import tarfile
import urllib.request
import zipfile

DEJAVU = ("https://github.com/dejavu-fonts/dejavu-fonts/releases/download/version_2_37/dejavu-fonts-ttf-2.37.tar.bz2", "fa9ca4d13871dd122f61258a80d01751d603b4d3ee14095d65453b4e846e17d7")
NOTO = ("https://github.com/notofonts/devanagari/releases/download/NotoSansDevanagari-v2.007/NotoSansDevanagari-v2.007.zip", "820c7da45b1e63562cb41c0a8cac5d9a4202312043a3a040ed1325857ef469b1")
FILES = {
    "DejaVuSans.ttf": "7da195a74c55bef988d0d48f9508bd5d849425c1770dba5d7bfc6ce9ed848954",
    "DejaVuSans-Bold.ttf": "e6476c1b80502924294eed40894c5b18e06c181444ca953e5334262df9c27724",
    "DejaVuSans-Oblique.ttf": "4af75fa16ee6d3ad43e1ecec41862c24954af26a55c6bb1ebb27bd486a50f5f4",
    "DejaVuSans-BoldOblique.ttf": "eb436dca0c2594b73d8b603b892e374fdfd8d885d25ffb4f18df4c4c0b49e50f",
    "DejaVuSansMono.ttf": "b4a6c3e4faab8773f4ff761d56451646409f29abedd68f05d38c2df667d3c582",
    "DejaVuSerif.ttf": "42d1edeb7952f31b1f96d767ed7030b08a39e0c372b0071641518864e2bffb51",
    "NotoSansDevanagari-Regular.ttf": "9c7d935139ea6a1e6ad9dbac4f6d27ece1e04bca8123c8888d00a0f9df4724cd",
}


def verified(data, expected, name):
    actual = hashlib.sha256(data).hexdigest()
    if actual != expected:
        raise SystemExit(f"{name}: SHA-256 {actual}, expected {expected}")
    return data


def fetch(spec, local):
    url, sha = spec
    if local:
        data = Path(local).read_bytes()
    else:
        with urllib.request.urlopen(url, timeout=120) as response:
            data = response.read(32 * 1024 * 1024 + 1)
    return verified(data, sha, url)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, default=Path.home() / "projects/peartube-media-corpus/fonts/subs")
    parser.add_argument("--dejavu-archive", help="Use an already downloaded archive; still verify its hash")
    parser.add_argument("--noto-archive", help="Use an already downloaded archive; still verify its hash")
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=True)
    with tarfile.open(fileobj=io.BytesIO(fetch(DEJAVU, args.dejavu_archive))) as archive:
        for name, sha in FILES.items():
            if name.startswith("DejaVu"):
                data = archive.extractfile("dejavu-fonts-ttf-2.37/ttf/" + name).read()
                (args.directory / name).write_bytes(verified(data, sha, name))
        (args.directory / "DejaVu-LICENSE").write_bytes(archive.extractfile("dejavu-fonts-ttf-2.37/LICENSE").read())
    with zipfile.ZipFile(io.BytesIO(fetch(NOTO, args.noto_archive))) as archive:
        name = "NotoSansDevanagari-Regular.ttf"
        data = archive.read("NotoSansDevanagari/unhinted/ttf/" + name)
        (args.directory / name).write_bytes(verified(data, FILES[name], name))
        (args.directory / "Noto-OFL.txt").write_bytes(archive.read("OFL.txt"))
    print(f"Verified {len(FILES)} fonts in {args.directory}")
    print(f"SUBTITLE_TEST_FONTS={args.directory}")


if __name__ == "__main__":
    main()
