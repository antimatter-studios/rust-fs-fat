### Fixed
- A FAT volume whose boot sector has lost its `0x55 0xAA` signature but whose BPB is intact opens for reading, as it does for `fsck.fat` and the Linux kernel, instead of being refused; `BootSector::signature_ok`, `BootSector::warnings()` and `Volume::warnings()` report the missing signature, and `fs.fat info` shows it while the read verbs print a warning on stderr and exit 0.
- Mounting such a volume for writing is refused with an error that names the signature and how to repair it, and the formatter writes the signature in every boot sector, the FAT32 backup included.
