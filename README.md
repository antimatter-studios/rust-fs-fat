# rust-fs-fat

The repository contains the complete FAT-family driver. The Rust package remains
`rust-fs-fat32` and its library remains `fs_fat32` for compatibility with the existing
Captain Salvage integration. No separate driver is needed for FAT12 or FAT16.

Read-only FAT-family filesystem driver for FAT12, FAT16, and FAT32. One driver
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

`BootSector::parse` classifies the FAT variant using the standard data-cluster
thresholds. `FatKind` remains available to callers that need to report or
specialize on the on-disk format.

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

The initial extraction passes its synthetic FAT12/16/32 tests. Independent
formatter, reference-reader, and damaged real-volume validation are still pending.
Passing these tests is not a claim of production-ready recovery on arbitrary media.

## Development checks

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```
