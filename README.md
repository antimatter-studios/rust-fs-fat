# rust-fs-fat

The repository contains the complete FAT-family driver. The Rust package remains
`rust-fs-fat32` and its library remains `fs_fat32` for compatibility with the existing
Captain Salvage integration. No separate driver is needed for FAT12 or FAT16.

FAT-family filesystem driver for FAT12, FAT16, and FAT32, read-only unless a caller asks for a
writable mount. One driver
detects the variant from the BPB geometry; it does not treat the older formats
as FAT32. Variant-specific FAT entry encoding and root-directory layout are
handled behind shared APIs.

## API layers

The driver implements the three API layers every rust-fs driver shares, defined once in
`rust-fs-core` (`fs_core::layers`, cargo feature `layers`; `docs/three-layers.md` there is the
narrative). `fs_fat32::Volume` implements all of them, and `fs_fat32::layers` holds what is
specific to FAT behind them: the boot sector and its FAT32 backup, both FAT copies, cluster
chains, and directory entries with long names and code page 437 short names.

- **`VolumeAPI` and `OpenVolume`**: `Volume::probe` says whether a source holds FAT without
  opening it (`Yes`, `Damaged`, `No`), `open` takes a volume whose boot sector validates, and
  `open_for_salvage` also takes one whose primary boot sector is gone when the FAT32 backup boot
  sector survives, or whose image is shorter than it declares. `format()` is `fat12`, `fat16` or
  `fat32`; `geometry()`, `superblock()` (the decoded `BootSector`), `superblocks()` (primary and
  FAT32 backup), `support()` and the quick `health()` verdict follow the contract.
- **`PrimitiveAPI` and `SalvageAdapter`**: read-only by type, damage-tolerant. `structures()`
  lists the boot sectors, FSInfo, each FAT copy and the root directory. `FatAdapter` holds
  geometry and no device, and answers `find_candidates` (every directory entry of every directory
  cluster, live or deleted, with its long name, checksum state and times), `walk_extents` (the FAT
  chain, or for a deleted entry a guess: the next free clusters from its first, skipping clusters live files use, or with `DeletedGuess::Contiguous` a plain run; a free cluster another deleted file used since is a known limit), `rebuild_tree`,
  `report_damage` (boot sectors, FAT copies against each other, every FAT entry, each chain
  against its size, cross-links, lost clusters), `read_extent` and `cluster_layout`;
  materialising a plan is core's. Not offered: `EncodedExtents` (FAT stores nothing encoded) and
  `GeometryHypotheses` (a FAT12 or FAT16 volume keeps no backup boot sector to build one from).
  The FAT-specific reads stay inherent methods of `Volume`: `fat_entry_raw`, `fat_value`,
  `chain_candidates`.
- **`FileAPI`**: `root`, `read_dir`, `metadata` (attributes and the three FAT times, read as
  UTC), `read_at`, with core's `lookup`, `copy_to` and the case-insensitive `names_match` FAT
  needs. It does not write or repair the volume.
- `write::WriteVolume` is the only way to change a volume (see below).

The traits this crate had before (`volume::VolumeAPI`, `primitives::PrimitiveAPI`,
`file::FileAPI`) stay for one release as `#[deprecated]` wrappers over the same reads, and
`RawVolume` and `RawWriteVolume` are unchanged. `tests/layers_conformance.rs` runs rust-fs-core's
conformance checks on healthy and damaged FAT12, FAT16 and FAT32 volumes, and
`tests/layers_oracle.rs` holds the layers' verdicts against what the kernel, mtools and
`fsck.fat` say about the same images.

A directory cluster the tree from the root does not reach is recognised on its own bytes, so a scan
returns the same records however its range is cut. It is one when it opens with `.` and `..` naming
itself, or when its content is a run of directory entries (`FoundBy::Content`): every 32-byte slot
is empty, a long-name entry, or a short entry with allowed name characters, plausible attributes, a
first cluster inside the volume, a size that fits, and real dates; and there are at least two short
entries, or one whose long-name entries carry its checksum, or (the weakest evidence, marked
`FatDetail::lone_entry`) one short entry with every other slot empty. File data that resembles one
entry among other bytes is not accepted. Entries found by content are `Integrity::Unchecked` (reachable ones are `Verified`),
live when the cluster is allocated and deleted when it is free. A lost directory's later clusters
are found the same way, each on its own. The cost is a read of every data cluster in the scanned
range that opens with a plausible entry.

### A missing `0x55 0xAA` boot signature

Readers are lenient about it and writers are strict. `fsck.fat` (dosfstools 4.2) and the Linux
kernel's vfat driver (6.1) accept a volume whose BPB is intact and whose bytes 510 and 511 are
zero: `fsck.fat -n` exits 0 and says nothing about the signature (on FAT32 it only notes that the
boot sector differs from its backup), the kernel mounts the volume read-only and read-write, and
mtools lists it. So `Volume::open`, `open_device`, `open_path` and the `FileAPI` and `PrimitiveAPI`
on them accept such a volume. They report the defect instead of hiding it: `BootSector::signature`
and `signature_ok`, and `Volume::warnings()` (or `BootSector::warnings()`), which return a list of
`BootWarning` (`MissingSignature { found }`). Nothing is repaired and nothing is written.

The BPB is then the only evidence that the sector is a boot sector, so every geometry check still
applies, and an unsigned sector must also carry a media byte of 0xF0 or 0xF8 to 0xFF, which the
kernel and mtools require too (both refuse a media byte of 0 whatever the signature). A sector
that fails any check, an all-zero sector included, is refused as before, with `missing boot-sector
signature`. `RawVolume` needs no boot sector and is unchanged.

Writing is strict: `WriteVolume::mount`, `open_writable` and `open_writable_path` refuse an
unsigned volume with `Error::Invalid` naming the signature and how to repair it (restore bytes 510
and 511, from the backup boot sector on FAT32), and `mkfs::format` writes the signature in every
boot sector it writes (sector 0, the FAT32 backup at sector 6, and the end of a sector larger than
512 bytes). There is no silent repair; `RawWriteVolume` can write the two bytes. `FatKind` remains available to callers that need to report or
specialize on the on-disk format.

## Write capability

Reading is safe by construction: nothing a reader holds can write. `Volume`, `RawVolume`,
`FileAPI` and `PrimitiveAPI` need only `fs_core::BlockRead`, and the read-only constructors
(`open_device`, `open_raw_device`, `open_path`) wrap what they are given in a `ReadOnlyDevice`, so a
device that could write is still never written through them. `tests/read_only_contract.rs` runs every
read path, the error paths and a read that stops answering at each point, over a device that records
any write, flush or resize, and requires none.

A writer is reached only through an explicit constructor that needs a writable
`fs_core::BlockDevice`: `write::open_writable`, `write::open_writable_path` or
`WriteVolume::mount`. Mounting checks the geometry as reading does, and refuses (`Error::Dirty`)
a volume whose dirty or error flags are set, because only a tool that checks and repairs a volume
may clear them. For FAT16 and FAT32 those flags are the boot sector's state byte and the top bits of
FAT entry 1; FAT12 has none, so a FAT12 volume cannot be checked for them. The mount sets both to
dirty and flushes before its first change, and `unmount` flushes and clears them last, so a crash
leaves the volume marked dirty. Mounting and closing without a change writes nothing. A change that
fails after it wrote something leaves the mount failed and the volume dirty. Every write is bounded
to the volume, is made as whole sectors, and reaches the device as it is made; `flush` and `unmount`
ask the device to put it on stable storage.

`WriteVolume` assumes it is the only user of the volume: a kernel mount or another process writing
at the same time corrupts it, and it cannot tell. The caller keeps exclusive access.

`write::open_raw_writable` and `RawWriteVolume` are a separate, low-level capability for tools that
build or repair a volume byte by byte. It is not a mount: nothing is validated or flagged, and it
writes any bytes anywhere in its range.

## File operations

A `WriteVolume` creates, writes, truncates, renames and removes files and directories on FAT12,
FAT16 and FAT32, by path (`/` separated, names compared without regard to case): `create_file`,
`mkdir`, `write`, `truncate`, `rename`, `unlink`, `rmdir`, and `read_dir`, `stat`, `open`, `read` and
`len` to look at the result. Reads see every change as it is made. `FileHandle` ties a file to the
volume that opened it and to the directory entry it was opened on: another volume's handle is
refused (`ForeignHandle`), and once the file is removed, replaced or renamed the handle is refused
too (`StaleHandle`), so it cannot reach whatever is later stored in that slot or in a recycled
directory cluster.

- Names: a name that is already 8.3 in one case is a single short entry with the Windows NT
  lower-case flags. Any other name is long-name entries (with checksum and ordinals) over a short alias
  that gets a `~N` tail when making it lost something or the plain alias is taken (`Readme.md` is
  `README.MD` with no tail; `The quick brown fox.txt` is `THEQUI~1.TXT`). Characters that cannot be
  stored in a short name become `_`. Names up to 255 UTF-16 units are accepted; `.`/`..` parts, a
  trailing dot or space, control characters and `\ / : * ? " < > |` are refused with
  `InvalidName`.
- Space: each operation checks it has the clusters it needs before it writes, so a full volume
  (`VolumeFull`) or a full fixed FAT12/FAT16 root (`RootDirectoryFull`) is refused with nothing
  changed. Subdirectories and the FAT32 root grow by clusters, up to 65536 entries (`DirectoryFull`).
- Consistency: every FAT copy is written (FAT12's packed 12-bit entries are patched across sector
  boundaries), the FAT32 FSInfo free count and next-free hint are kept current, and each operation
  writes in an order that leaves the volume consistent if it stops (growing: FAT, data, then the
  entry; shrinking and removing: the entry, then the FAT). New and grown space reads as zeros, never
  what an earlier file left there.
- Timestamps come from `set_clock` (default the system clock, read as UTC; FAT keeps no zone).
  A file's size is at most 4 GiB less one byte (`FileTooLarge`).
- Not done: file attributes cannot be set, read-only files are not protected, short-name hashing
  after many collisions (a `~N` counter is used), and a device whose logical block is larger than the
  volume's sectors is not supported.

`tests/write_ops.rs` runs these on all three variants against `tests/support/check.rs`, a parser
written for the tests that does not use the driver, and compares a 400-step random sequence with a
model. The operations are also validated against other implementations (see Validation status).

## Formatting

`mkfs::format` (and `format_path`, for an image file it creates or grows) writes an empty FAT12,
FAT16 or FAT32 volume of a given size, at an offset within the device if asked. The variant is not
a field of the volume: the number of data clusters decides it (up to 4084 is FAT12, 4085 to 65524 is
FAT16, 65525 or more is FAT32). Left to choose, `plan` picks FAT12 up to 16 MiB, FAT16 below
512 MiB and FAT32 from there, with the smallest cluster size that puts the count in range (the usual
cluster-size table for FAT32). A variant that is asked for and cannot form at that size is refused
with an error that says why, not turned into another. It writes the BPB, every FAT copy, an empty
root directory, and for FAT32 the FSInfo sector and the backup boot sector and FSInfo, and leaves the
data area alone. Defaults: 512-byte sectors, two FATs, 1 reserved sector (32 for FAT32), 512 root
entries for FAT12/16, media 0xF8, a serial from the clock.

`tests/format_oracle.rs` formats images from FAT12 through FAT32, including the cluster counts at
both ends of each variant (4084, 4085, 65524 and 65525), 1024- and 4096-byte sectors and one FAT
copy, then requires `fsck.fat -n` to report each clean and the Linux kernel's vfat driver to mount it
read-only with the cluster size, cluster count and free count the layout planned.

Run the driver tests with:

```sh
cargo test -p rust-fs-fat32
```

The normal filesystem API stops at directory end markers and excludes deleted
entries. `directory::scan_directory` is the forensic primitive: it retains raw
records and physical offsets, including deleted entries and records beyond an
end marker. Long names require their sequence and short-name checksum to agree.
Deleted short names use an underscore for the missing first character; a deleted
long-name sequence is not guessed.

Captain Salvage adapts those records into its shared scan, map, browse, and
recovery model. FAT12/16 fixed roots are indexed separately from data clusters;
FAT32 roots follow their cluster chains. Allocation comes from the active FAT,
not an invented on-disk bitmap. Cleared chains remain partial unless the person
explicitly enables the shared contiguous-fallback hypothesis.

## The command-line tool

`rust-fs-fat` is one multi-call binary behind the `cli` cargo feature, so the library
keeps no CLI dependency: `cargo build` builds the library alone. It dispatches on the
name it is started as, the way the sibling drivers' tools do, and `rust-fs-fat fs ...`
is the form nothing else on `PATH` can shadow. Its plumbing (dispatch, `--version`,
`doctor`, the JSON-or-`--text` output, the structured error and exit statuses, man
pages and completions) is `rust-fs-core`'s `cli` feature, the same for every driver.

```sh
cargo install rust-fs-fat32 --features cli
fs.fat disk.img info                      # boot sector, variant, backup boot sector
fs.fat disk.img ls -R /docs               # live entries; --limit and --max-depth bound the walk
fs.fat disk.img cat /docs/guide.txt       # raw bytes on stdout
fs.fat disk.img export /docs/guide.txt guide.txt --verify
fs.fat disk.img scan --deleted -R /       # deleted records, raw bytes and where they are on disk
fs.fat disk.img chain 7                   # the FAT chain from cluster 7, and why it stops
fs.fat disk.img fat 2 --count 8           # FAT entries as stored
fs.fat damaged.img info                   # what is wrong with the boot sector, and the backup
fs.fat damaged.img --boot-sector 6 ls /   # read the volume through the FAT32 backup boot sector
```

Every verb opens the image read-only (a `FileDevice` wrapped again in `ReadOnlyDevice`)
and never writes to it. `info` goes through `RawVolume` and needs no valid boot sector, so
it answers for a damaged one; every other verb goes through the checked `Volume`, and
refuses a volume whose boot sector does not hold up with exit 4, naming `--boot-sector`.
A volume whose only defect is a missing `0x55 0xAA` signature is read: `info` says `valid: true`
with the defect under `warnings`, `boot_sector.signature_valid: false` and, for FAT32,
`backup.signature_valid`; every other verb prints one `warning:` line on stderr and exits 0 with
the same stdout as for a signed volume. `--boot-sector SECTOR` substitutes the boot sector found at that
512-byte sector for sector 0, which is how a FAT32 volume with a destroyed sector 0 is
read through its backup copy at sector 6. FAT12 and FAT16 keep no backup boot sector.
The library lists only the root directory, so subdirectories and paths are resolved in
the tool from `chain_candidates`, `read_cluster` and `parse_directory`; the library is
unchanged.

Output and exit statuses follow the shared contract: a result is JSON on stdout (or
`--text`), a failure is `{"error": ..., "code": N}` on stderr, and `N` is the exit status
(1 failed or not found, 2 wrong command line, 4 refused because of the volume's state).
`scan` reports what the library's `scan_directory` keeps; it does not claim a deleted
file's data is intact, and a deleted file's first cluster reads as free in the FAT.

### The names, and why `fs.fat`

dosfstools installs `mkfs.fat`, `mkfs.vfat`, `mkfs.msdos`, `fsck.fat`, `fsck.vfat`,
`fsck.msdos`, `fatlabel` and `dosfslabel`, and a host that has it must keep them. So none of
those names is used, and the tool shipped here is `fs.fat` (manual section 1), which is in
neither dosfstools nor mtools: `tests/cli_independent.rs` checks both packages' file lists
in the Linux test VM, and `scripts/test-cli.sh` runs `rust-fs-fat doctor` first so a
shadowed name fails the tier. The release metadata (`[package.metadata.package-cli]`) lists
exactly `fs.fat`, and the packaging step fails if the binary answers to another name.

`mkfs`, file operations and the checker are separate work (#2 to #5) and are not part of
this tool yet. Their dotted names are not chosen here beyond the constraint that they must
not be any name above: `rust-fs-fat mkfs` and `rust-fs-fat fsck` work as verbs of the
multi-call binary whatever dotted name they get. File operations are further verbs of
`fs.fat` (one entry in `VERBS` in `src/cli/fs.rs`, one module, opening its own writable
device); the formatter and the checker are further tools in `FAMILY.tools` in
`src/cli/main.rs`.

### Dependencies and licences

Without the `cli` feature the dependencies are `rust-fs-core` (MIT) with its `layers` feature, and `serde` (MIT or Apache-2.0), which the salvage evidence is serialised with; `layers` adds sha2 (MIT or Apache-2.0). The feature adds
`rust-fs-core`'s `cli` dependencies, all permissive: clap, clap_complete and clap_mangen
(MIT or Apache-2.0), serde, serde_json and sha2 (MIT or Apache-2.0), signal-hook
(Apache-2.0 or MIT), xattr, rustix, libc, errno and the other crates they pull in (MIT or
Apache-2.0; memchr and winapi-util are Unlicense or MIT; strsim and zmij are MIT;
unicode-ident adds Unicode-3.0). None is copyleft.

## Validation status

### Reading

The driver passes its synthetic FAT12/16/32 tests, and
`tests/independent_validation.rs` checks it against `mkfs.fat`, `fsck.fat`, mtools and
the Linux kernel's vfat driver on images those tools built (clean, with deleted files, a
truncated chain, and damaged boot sectors, among them a zeroed signature): listings, file bytes,
cluster chains including fragmented files, and that nothing is written. `tests/cli_independent.rs`
runs the command-line tool as a process on the same images and compares it with the same
oracles; `tests/cli.rs` and `tests/cli/` run it on two small volumes formatted by
`mkfs.fat` and filled by mtools (`validation/make-cli-samples.sh`). The oracle tests run in
the Linux test VM (`chore test:oracle`) and in CI's `oracle` job. The formatter is checked the same way by
`tests/format_oracle.rs` (see Formatting).

Not validated for reading: any Windows tool (Windows' view of written volumes is in Writing below), the macOS FAT implementation, FAT with 1024-, 2048- or
4096-byte sectors, volumes larger than the 40 MiB fixtures, or damage beyond the recipes in
`validation/generate.sh`. A zeroed `0x55 0xAA` signature (#8) is validated for FAT12, FAT16 and
FAT32: `fsck.fat -n` exits 0, the kernel mounts the image and mtools lists it
(`<image>.verdict`), and the driver and `fs.fat` list the same paths and read the same bytes and
cluster chains as the kernel and mtools, with a warning and without writing. The same zeroed
signature with a media byte of 0 is refused by the kernel, by mtools and by the driver; with a
sectors-per-FAT field of 1 the driver refuses it where the kernel still mounts it (the driver
checks that the FAT can hold the data region). Passing these tests is not a claim of production-ready recovery on arbitrary
media.

### Writing

Volumes this driver formats and writes are checked by implementations that do not share its code.
`validation/inspect-image.sh` (Linux test VM or a Linux runner) takes each image after a clean close
and records what `fsck.fat -n` (dosfstools), the Linux kernel's vfat driver (a read-only loop mount) and
mtools (`mcopy`, `mdir`) make of it; `tests/write_oracle.rs` requires all three to accept it and to agree
with the driver on every name, size, content hash, modification time, creation time (to the 10 ms the
kernel reports) and access date.

- FAT12, FAT16 and FAT32 each run a 20-step scenario covering every operation (empty, small and
  multi-cluster files, nested directories, NT-case names, long names with and without a short-name
  collision, Unicode names, overwrite, append and sparse writes, truncate shorter, longer and to zero,
  rename within a directory, across directories and of a directory, unlink, rmdir, fragmentation,
  directory growth, FAT32 root growth) with `fsck.fat -n` clean after **each** step, 63 images in all.
- A full volume and a full fixed FAT12/FAT16 root directory (exact capacity) are clean too.
- Volumes the kernel built and filled (`validation/generate.sh`, nested directories, long and
  Unicode names, NT flags, a chained FAT32 root) are read back by the driver, changed by it, and checked
  again. In the other direction the kernel writes into volumes the driver wrote (`validation/kernel-touch.sh`:
  long, short and Unicode names, an append, a rename, a delete, 20 more files), the driver reads that, writes
  over it, and `fsck.fat` and the kernel agree each time.
- Damage recipes (a differing FAT copy, an unowned cluster, a cut chain, a size larger than the chain, a
  looping chain, a left-set dirty bit) are generated by the test; `fsck.fat` must reject every one, so a
  clean verdict above is not vacuous.
- Windows: `chore test:windows` (`tests/windows_oracle.rs`, `validation/windows-check.ps1`) formats and writes
  FAT12, FAT16 and FAT32 volumes inside an MBR-partitioned disk at a 1 MiB offset, attaches each as a VHD in
  the Windows test VM of `fs-windows-test-harness` (Windows 11 ARM64, build 26100), runs `chkdsk` without `/F`,
  and requires exit 0 and a listing with the same names, sizes, hashes and times; two damage recipes must be
  rejected by `chkdsk`. This runs on a developer machine with that VM, not in CI (no Windows runner is wired
  into this repository's pipeline).

Tool versions are recorded with each run: `tools.txt` beside the images (kept as a CI artifact) holds
the kernel, dosfstools and mtools versions; the Windows build is printed by the Windows test. The values
at the time of writing were dosfstools 4.2-1, mtools 4.0.33, Linux 6.1.0-42 (arm64).

Images and damage are generated by the tests and scripts on every run and never committed.

Not validated for writing: FAT with 1024-, 2048- or 4096-byte sectors written by the driver (formatted and
checked, but not written), volumes larger than the generated 34 MiB, a device whose logical block is larger
than the volume sector, volumes written under macOS's FAT implementation, a power loss between the writes of an
operation (the dirty flags and the write order are designed for it but it is not injected against an
oracle), and real hardware. Passing these tests is not a claim of production-ready writing on arbitrary media.

## Development checks

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features cli -- -D warnings
cargo build --locked                    # the library alone, with no CLI dependency
cargo test --locked --features cli      # the library, the tool's unit tests, tests/cli.rs
chore test:cli                          # the installed tool: doctor, then tests/cli/test-*.sh
chore test:oracle                       # the independent oracles, in the Linux test VM
```
