### Added
- `WriteVolume` creates, writes, truncates, renames and removes files and directories on FAT12, FAT16 and FAT32 (`create_file`, `mkdir`, `write`, `truncate`, `rename`, `unlink`, `rmdir`, with `read_dir`, `stat`, `open`, `read` and `len`).
- Short names get the generated `~N` tail where needed, long names carry the checksum and sequence, FAT12 packed entries are updated across sector boundaries, every FAT copy and the FAT32 FSInfo hints are kept current, and a full volume or full fixed root directory is refused with nothing changed.
- `FileHandle` is tied to its volume and to the directory entry it opened, so it cannot reach a recycled slot.
