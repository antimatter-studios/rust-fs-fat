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

## Validation status

The driver is checked against tools that do not share its code. `validation/generate.sh` builds
FAT12, FAT16 and FAT32 images with `mkfs.fat` (dosfstools), fills them through the Linux kernel's
vfat driver, and requires the kernel's view and an `mtools` (`mcopy`) copy to agree byte for byte.
`tests/independent_validation.rs` then compares the driver with them: directory listings (nested
directories, long and non-ASCII names, NT lower-case flags, a FAT12/16 fixed root, a chained FAT32
root), file bytes, and cluster chains against `mshowfat`, including fragmented files. It also
checks that each image is byte-identical after being read, on the error paths too.

Damaged copies are built by recorded recipes (`generate.sh` lists them): deleted records, a
truncated cluster chain, a missing boot signature, a zeroed boot sector, and a zero bytes-per-sector
field. `fsck.fat` is the oracle that the damage is real. `RawVolume` reads every byte of each of
them; `Volume` refuses the three boot-sector recipes, and refuses only the damaged file when a
chain is truncated.

Images are rebuilt from the recipe on each run and never committed. Run the tests in the Linux
test VM with `chore test:oracle` (sibling `../fs-linux-test-harness`), or natively as root on Linux
with `FAT_ORACLE_NATIVE=1 cargo test --features oracle --test independent_validation`; CI does the
latter in its `oracle` job. Without the oracles those tests fail, they do not skip.

Not yet validated: images with a non-zero partition offset or a 4096-byte sector, volumes larger
than the generated 40 MiB, damaged FATs beyond one truncated chain, directory-loop and
cross-linked-chain damage, and real damaged media. `Volume` is stricter than `fsck.fat` and the
kernel about the boot signature: both accept an image whose `0x55 0xAA` is zeroed, `Volume::open`
does not (use `RawVolume` for such a volume). Passing these tests is not a claim of
production-ready recovery on arbitrary media.

## Development checks

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```
