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

- `VolumeAPI` reads raw volume bytes; `RawVolume` works even when the boot
  sector is damaged, while `Volume` adds checked BPB geometry.
- `PrimitiveAPI` decodes raw FAT entries and returns best-effort cluster-chain
  candidates, stopping safely at broken links, special values, loops, and
  caller-supplied limits.
- `FileAPI` lists valid root entries and opens files for bounded random-access
  reads. It does not write or repair the volume.
- `write::WriteVolume` is the only way to change a volume (see below).

`BootSector::parse` classifies the FAT variant using the standard data-cluster
thresholds. `FatKind` remains available to callers that need to report or
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
refuses a volume whose boot sector does not hold up with exit 4, naming `--boot-sector`. `--boot-sector SECTOR` substitutes the boot sector found at that
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

Without the `cli` feature the only dependency is `rust-fs-core` (MIT). The feature adds
`rust-fs-core`'s `cli` dependencies, all permissive: clap, clap_complete and clap_mangen
(MIT or Apache-2.0), serde, serde_json and sha2 (MIT or Apache-2.0), signal-hook
(Apache-2.0 or MIT), xattr, rustix, libc, errno and the other crates they pull in (MIT or
Apache-2.0; memchr and winapi-util are Unlicense or MIT; strsim and zmij are MIT;
unicode-ident adds Unicode-3.0). None is copyleft.

## Validation status

The driver passes its synthetic FAT12/16/32 tests, and
`tests/independent_validation.rs` checks it against `mkfs.fat`, `fsck.fat`, mtools and
the Linux kernel's vfat driver on images those tools built (clean, with deleted files, a
truncated chain, and three kinds of damaged boot sector): listings, file bytes, cluster
chains including fragmented files, and that nothing is written. `tests/cli_independent.rs`
runs the command-line tool as a process on the same images and compares it with the same
oracles; `tests/cli.rs` and `tests/cli/` run it on two small volumes formatted by
`mkfs.fat` and filled by mtools (`validation/make-cli-samples.sh`). The oracle tests run in
the Linux test VM (`chore test:oracle`) and in CI's `oracle` job. The formatter is checked the same way by
`tests/format_oracle.rs` (see Formatting).

Not validated: any Windows tool, the macOS FAT implementation, FAT with 1024-, 2048- or
4096-byte sectors, volumes larger than the 40 MiB fixtures, or damage beyond the recipes in
`validation/generate.sh`. `fsck.fat` accepts a boot sector whose `0x55 0xAA` signature is
zero and `Volume::open` refuses it (#8); the tool reports that volume as not valid, like the
library does. Passing these tests is not a claim of production-ready recovery on arbitrary
media.

## Development checks

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features cli -- -D warnings
cargo build --locked                    # the library alone, with no CLI dependency
cargo test --locked --features cli      # the library, the tool's unit tests, tests/cli.rs
chore test:cli                          # the installed tool: doctor, then tests/cli/test-*.sh
chore test:oracle                       # the independent oracles, in the Linux test VM
```
