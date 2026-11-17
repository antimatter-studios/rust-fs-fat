### Added
- The `rust-fs-fat` command-line tool, behind the `cli` feature, with one tool, `fs.fat`, that only reads: `info`, `ls`, `stat`, `cat`, `export`, `scan`, `chain` and `fat` work on a FAT12, FAT16 or FAT32 image or device without mounting it.
- `fs.fat IMAGE info` reports the boot sector and variant of a volume whose boot sector is damaged, and where a backup copy is; `--boot-sector SECTOR` makes the other verbs read the volume through that copy.
- `fs.fat IMAGE scan` dumps a directory's forensic records, deleted entries and records past the end marker included, with their raw bytes and their place on disk.
- A release tarball with the multi-call binary, `fs.fat`, man pages and shell completions, built by `rust-fs-core`'s package script.
