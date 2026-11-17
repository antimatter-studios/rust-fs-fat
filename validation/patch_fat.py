#!/usr/bin/env python3
"""Set one FAT entry, in every FAT copy, of a FAT12/16/32 image.

usage: patch_fat.py IMAGE KIND CLUSTER VALUE
KIND is 12, 16 or 32. VALUE is decimal or 0x-hex. Only the entry's own bits
change; the high nibble of a FAT32 entry is kept. Part of the reproducible
damage recipes in generate.sh.
"""
import struct
import sys


def main():
    image, kind, cluster, value = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4], 0)
    with open(image, "r+b") as f:
        boot = f.read(512)
        bps = struct.unpack_from("<H", boot, 11)[0]
        reserved = struct.unpack_from("<H", boot, 14)[0]
        nfats = boot[16]
        spf = struct.unpack_from("<H", boot, 22)[0] or struct.unpack_from("<I", boot, 36)[0]
        for copy in range(nfats):
            base = (reserved + copy * spf) * bps
            if kind == 12:
                off = base + cluster * 3 // 2
                f.seek(off)
                word = struct.unpack("<H", f.read(2))[0]
                if cluster & 1:
                    word = (word & 0x000F) | ((value & 0xFFF) << 4)
                else:
                    word = (word & 0xF000) | (value & 0xFFF)
                f.seek(off)
                f.write(struct.pack("<H", word))
            elif kind == 16:
                f.seek(base + cluster * 2)
                f.write(struct.pack("<H", value & 0xFFFF))
            else:
                f.seek(base + cluster * 4)
                old = struct.unpack("<I", f.read(4))[0]
                f.seek(base + cluster * 4)
                f.write(struct.pack("<I", (old & 0xF0000000) | (value & 0x0FFFFFFF)))


main()
