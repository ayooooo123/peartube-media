#!/usr/bin/env python3
"""Fetch members of a remote ZIP over HTTP range requests, without
downloading the whole archive (the ISO USAC conformance set is 3.5 GB).

usage: remote_zip.py <url> index <index.json>      central directory -> JSON
       remote_zip.py <index.json> fetch <member>... into ./members/, CRC-checked

`fetch` downloads each member's local header and compressed bytes with curl
(retries, resumable), then inflates and verifies the member's CRC-32.
"""
import json
import os
import struct
import subprocess
import sys
import urllib.request
import zipfile
import zlib


class RemoteFile:
    BLOCK = 1 << 18

    def __init__(self, url):
        self.url = url
        request = urllib.request.Request(url, method="HEAD")
        with urllib.request.urlopen(request, timeout=120) as response:
            self.size = int(response.headers["Content-Length"])
        self.pos = 0
        self.cache = {}

    def seekable(self):
        return True

    def seek(self, offset, whence=0):
        self.pos = {0: offset, 1: self.pos + offset, 2: self.size + offset}[whence]
        return self.pos

    def tell(self):
        return self.pos

    def _block(self, index):
        if index not in self.cache:
            start = index * self.BLOCK
            end = min(self.size, start + self.BLOCK) - 1
            data = subprocess.run(["curl", "-s", "--retry", "8", "--retry-all-errors", "-m", "600",
                                   "-r", f"{start}-{end}", self.url], check=True, capture_output=True).stdout
            assert len(data) == end - start + 1, (len(data), start, end)
            self.cache[index] = data
        return self.cache[index]

    def read(self, n=-1):
        if n < 0:
            n = self.size - self.pos
        n = min(n, self.size - self.pos)
        out = bytearray()
        while n > 0:
            index, offset = divmod(self.pos, self.BLOCK)
            chunk = self._block(index)[offset:offset + n]
            out += chunk
            self.pos += len(chunk)
            n -= len(chunk)
        return bytes(out)


def index(url, path):
    archive = zipfile.ZipFile(RemoteFile(url))
    members = {info.filename: {"offset": info.header_offset, "compressed": info.compress_size,
                               "size": info.file_size, "method": info.compress_type, "crc": info.CRC}
               for info in archive.infolist()}
    json.dump({"url": url, "members": members}, open(path, "w"), indent=0)
    print(f"{len(members)} members")


def fetch(index_path, names):
    catalog = json.load(open(index_path))
    url = catalog["url"]
    for name in names:
        info = catalog["members"][name]
        target = os.path.join("members", name)
        if os.path.exists(target) and zlib.crc32(open(target, "rb").read()) == info["crc"]:
            print(f"{name}: present")
            continue
        os.makedirs(os.path.dirname(target), exist_ok=True)
        part = target + ".part"
        # Local header (30 bytes + name + extra) precedes the data; 64 KiB of
        # slack covers any extra field.
        start = info["offset"]
        end = start + 30 + len(name.encode()) + 65536 + info["compressed"]
        subprocess.run(["curl", "-s", "--retry", "8", "--retry-all-errors", "-m", "3600",
                        "-C", "-", "-r", f"{start}-{end}", "-o", part, url], check=True)
        raw = open(part, "rb").read()
        signature, _, _, method, _, _, _, compressed, _, name_len, extra_len = struct.unpack("<IHHHHHIIIHH", raw[:30])
        assert signature == 0x04034B50, name
        begin = 30 + name_len + extra_len
        data = raw[begin:begin + info["compressed"]]
        assert len(data) == info["compressed"], (name, len(data))
        payload = data if info["method"] == 0 else zlib.decompress(data, -15)
        assert zlib.crc32(payload) == info["crc"] and len(payload) == info["size"], name
        open(target, "wb").write(payload)
        os.remove(part)
        print(f"{name}: {len(payload)} bytes, CRC {info['crc']:08x}")


def main():
    if sys.argv[2] == "index":
        index(sys.argv[1], sys.argv[3])
    else:
        fetch(sys.argv[1], sys.argv[3:])


if __name__ == "__main__":
    main()
