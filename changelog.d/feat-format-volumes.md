### Added
- `mkfs::format` and `mkfs::format_path` format FAT12, FAT16 and FAT32 volumes of a given size, choosing the variant from the size and the standard cluster-count thresholds, and refusing sizes and parameters that cannot form the requested variant.
- `timestamp::FatTimestamp` converts FAT dates and times.
