### Added
- `fs_fat32::Volume` implements the three shared API layers of `rust-fs-core` 0.5 (`fs_core::layers`): `VolumeAPI` and `OpenVolume` (`probe`, `open`, `open_for_salvage`, geometry, boot sector and its FAT32 backup, support, health), `PrimitiveAPI` with the salvage verbs (`structures`, and `FatAdapter`: `find_candidates`, `walk_extents`, `rebuild_tree`, `report_damage`, `read_extent`, `cluster_layout`) and `FileAPI` (`root`, `read_dir`, `metadata`, `read_at`, `lookup`, `copy_to`).
- A FAT volume whose primary boot sector is lost opens for salvage through its FAT32 backup boot sector, and an image shorter than its boot sector declares opens as far as it goes.
- `Volume::fat_entry_raw`, `fat_value` and `chain_candidates` are methods of the volume itself.
### Deprecated
- `volume::VolumeAPI`, `primitives::PrimitiveAPI` and `file::FileAPI` are superseded by the shared traits. They keep working for one release and are removed in the next.
