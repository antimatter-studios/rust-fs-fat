### Added
- `write::WriteVolume` mounts a FAT12, FAT16 or FAT32 volume for writing through an explicit constructor that needs a writable `fs_core::BlockDevice`. It refuses a volume marked dirty, marks the volume dirty before its first change, and clears the mark on a clean close. Every write is bounded to the volume.
- `write::RawWriteVolume` is a separate, explicit low-level byte-writing capability that is not a mount.
- The read-only constructors still never write, which a new contract test checks on every read path, including errors and failing reads.
