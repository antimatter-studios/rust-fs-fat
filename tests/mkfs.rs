//! The formatter: the layouts it chooses, what it writes, and what it refuses.

mod support;

use fs_fat32::file::FileAPI;
use fs_fat32::mkfs::{format, format_path, plan, FormatOptions};
use fs_fat32::volume::{open_device, open_path, VolumeAPI};
use fs_fat32::write::WriteVolume;
use fs_fat32::{Error, FatKind, Volume};
use support::Disk;

const MIB: u64 = 1024 * 1024;

fn options(kind: Option<FatKind>) -> FormatOptions {
    FormatOptions {
        kind,
        serial: Some(0xCAFE_F00D),
        ..FormatOptions::default()
    }
}

fn formatted(bytes: u64, options: &FormatOptions) -> (Disk, fs_fat32::mkfs::Layout) {
    let disk = Disk::new(vec![0xEE; bytes as usize]);
    let layout = format(&disk, options).unwrap();
    (disk, layout)
}

#[test]
fn the_variant_follows_the_size_and_the_cluster_count_thresholds() {
    let auto = |bytes: u64| plan(bytes, &options(None)).unwrap();
    assert_eq!(auto(1440 * 1024).kind, FatKind::Fat12);
    assert_eq!(
        auto(16 * MIB).kind,
        FatKind::Fat16,
        "too many clusters for 12"
    );
    assert_eq!(auto(15 * MIB).kind, FatKind::Fat12);
    assert_eq!(auto(100 * MIB).kind, FatKind::Fat16);
    assert_eq!(auto(511 * MIB).kind, FatKind::Fat16);
    assert_eq!(auto(512 * MIB).kind, FatKind::Fat32);
    assert_eq!(auto(40_000 * MIB).kind, FatKind::Fat32);
    for bytes in [1440 * 1024, 15 * MIB, 100 * MIB, 600 * MIB, 40_000 * MIB] {
        let layout = auto(bytes);
        let (low, high) = match layout.kind {
            FatKind::Fat12 => (1, 4084),
            FatKind::Fat16 => (4085, 65_524),
            FatKind::Fat32 => (65_525, u32::MAX),
        };
        assert!(
            (low..=high).contains(&layout.cluster_count),
            "{bytes}: {layout:?}"
        );
    }
    // The usual cluster sizes of a large FAT32 volume.
    assert_eq!(auto(600 * MIB).cluster_bytes(), 4096);
    assert_eq!(auto(10_000 * MIB).cluster_bytes(), 8192);
    assert_eq!(auto(40_000 * MIB).cluster_bytes(), 32768);
}

#[test]
fn every_variant_can_be_forced_where_it_forms_and_is_refused_where_it_cannot() {
    for (kind, bytes) in [
        (FatKind::Fat12, 2 * MIB),
        (FatKind::Fat16, 8 * MIB),
        (FatKind::Fat32, 40 * MIB),
    ] {
        assert_eq!(plan(bytes, &options(Some(kind))).unwrap().kind, kind);
    }
    let refusals = [
        (FatKind::Fat32, 10 * MIB, "too small"),
        (FatKind::Fat16, MIB, "too small"),
        (FatKind::Fat12, 400 * MIB, "too large"),
        (FatKind::Fat16, 4096 * MIB, "too large"),
    ];
    for (kind, bytes, why) in refusals {
        let error = plan(bytes, &options(Some(kind))).unwrap_err();
        assert!(
            matches!(&error, Error::Invalid(reason) if reason.contains(why)),
            "{kind:?} {bytes}: {error}"
        );
    }
}

#[test]
fn the_cluster_count_decides_the_variant_exactly_at_each_boundary() {
    // One sector per cluster, so every total size sets the count directly.
    let spc1 = |kind| FormatOptions {
        sectors_per_cluster: Some(1),
        ..options(Some(kind))
    };
    let mut seen = std::collections::BTreeSet::new();
    for (kind, boundary) in [
        (FatKind::Fat12, 4_084u64),
        (FatKind::Fat16, 4_085),
        (FatKind::Fat16, 65_524),
        (FatKind::Fat32, 65_525),
    ] {
        for total in boundary.saturating_sub(5)..boundary + 1_300 {
            let Ok(layout) = plan(total * 512, &spc1(kind)) else {
                continue;
            };
            seen.insert((kind as u8, layout.cluster_count));
            let reread =
                fs_fat32::BootSector::parse(&fs_fat32::mkfs::boot_sector(&layout)).unwrap();
            assert_eq!(reread.kind, kind, "{total}: {layout:?}");
        }
    }
    let has = |kind: FatKind, count: u32| seen.contains(&(kind as u8, count));
    assert!(has(FatKind::Fat12, 4084), "the largest FAT12");
    assert!(!has(FatKind::Fat12, 4085), "4085 clusters is never FAT12");
    assert!(has(FatKind::Fat16, 4085), "the smallest FAT16");
    assert!(!has(FatKind::Fat16, 4084));
    assert!(has(FatKind::Fat16, 65_524), "the largest FAT16");
    assert!(!has(FatKind::Fat16, 65_525));
    assert!(has(FatKind::Fat32, 65_525), "the smallest FAT32");
    assert!(!has(FatKind::Fat32, 65_524));
}

#[test]
fn what_is_written_reads_back_and_mounts_for_writing() {
    for (kind, bytes) in [
        (FatKind::Fat12, 1440 * 1024u64),
        (FatKind::Fat16, 20 * MIB),
        (FatKind::Fat32, 40 * MIB),
    ] {
        let (disk, layout) = formatted(bytes, &options(Some(kind)));
        let volume = Volume::open(disk.clone()).unwrap();
        let boot = volume.boot();
        assert_eq!(boot.kind, kind);
        assert_eq!(boot.cluster_count, layout.cluster_count);
        assert_eq!(boot.volume_serial, 0xCAFE_F00D);
        assert_eq!(boot.sectors_per_fat, layout.sectors_per_fat);
        assert_eq!(boot.total_sectors, layout.total_sectors);
        assert!(volume.root_directory().unwrap().is_empty());

        let image = disk.bytes();
        let sector = 512usize;
        let fat = |copy: usize| {
            let start = (usize::from(layout.reserved_sectors)
                + copy * layout.sectors_per_fat as usize)
                * sector;
            image[start..start + layout.sectors_per_fat as usize * sector].to_vec()
        };
        assert_eq!(
            fat(0),
            fat(1),
            "{kind:?}: every FAT copy is initialised alike"
        );
        let head = fat(0);
        match kind {
            FatKind::Fat12 => assert_eq!(&head[..3], &[0xF8, 0xFF, 0xFF]),
            FatKind::Fat16 => assert_eq!(&head[..4], &[0xF8, 0xFF, 0xFF, 0xFF]),
            FatKind::Fat32 => {
                assert_eq!(
                    &head[..12],
                    &[0xF8, 0xFF, 0xFF, 0x0F, 0xFF, 0xFF, 0xFF, 0x0F, 0xFF, 0xFF, 0xFF, 0x0F]
                );
            }
        }
        assert!(
            head[12..].iter().all(|b| *b == 0),
            "{kind:?}: the rest is free"
        );
        // A freshly formatted volume is clean, so a writer accepts it.
        drop(volume);
        WriteVolume::mount(disk.clone()).unwrap().unmount().unwrap();
        assert_eq!(
            disk.bytes(),
            image,
            "{kind:?}: mounting a new volume writes nothing"
        );
    }
}

#[test]
fn fat32_has_fsinfo_a_backup_boot_sector_and_a_root_cluster() {
    let (disk, layout) = formatted(40 * MIB, &options(Some(FatKind::Fat32)));
    let image = disk.bytes();
    let sector = |n: usize| &image[n * 512..(n + 1) * 512];
    assert_eq!(sector(0), sector(6), "backup boot sector");
    assert_eq!(sector(1), sector(7), "backup FSInfo");
    let info = sector(1);
    assert_eq!(
        u32::from_le_bytes(info[0..4].try_into().unwrap()),
        0x4161_5252
    );
    assert_eq!(
        u32::from_le_bytes(info[484..488].try_into().unwrap()),
        0x6141_7272
    );
    assert_eq!(
        u32::from_le_bytes(info[488..492].try_into().unwrap()),
        layout.cluster_count - 1,
        "every cluster but the root's is free"
    );
    assert_eq!(u32::from_le_bytes(info[492..496].try_into().unwrap()), 3);
    assert_eq!(&info[508..512], &[0, 0, 0x55, 0xAA]);
    let boot = sector(0);
    assert_eq!(u16::from_le_bytes([boot[48], boot[49]]), 1);
    assert_eq!(u16::from_le_bytes([boot[50], boot[51]]), 6);
    assert_eq!(u32::from_le_bytes(boot[44..48].try_into().unwrap()), 2);
    let volume = Volume::open(disk).unwrap();
    assert_eq!(volume.boot().root_cluster, 2);
    let mut root = vec![0u8; volume.boot().cluster_size() as usize];
    volume.read_cluster(2, &mut root).unwrap();
    assert!(root.iter().all(|b| *b == 0));
}

#[test]
fn only_metadata_is_written_and_the_data_area_is_left_alone() {
    for (kind, bytes) in [
        (FatKind::Fat12, 2 * MIB),
        (FatKind::Fat16, 20 * MIB),
        (FatKind::Fat32, 40 * MIB),
    ] {
        let (disk, layout) = formatted(bytes, &options(Some(kind)));
        let image = disk.bytes();
        let data = layout.first_data_sector as usize * 512;
        let root_skip = if kind == FatKind::Fat32 {
            layout.cluster_bytes() as usize
        } else {
            0
        };
        assert!(
            image[data + root_skip..layout.volume_bytes() as usize]
                .iter()
                .all(|b| *b == 0xEE),
            "{kind:?}"
        );
    }
}

#[test]
fn a_label_goes_in_the_boot_sector_and_the_root_directory() {
    for (kind, bytes) in [(FatKind::Fat16, 20 * MIB), (FatKind::Fat32, 40 * MIB)] {
        let (disk, layout) = formatted(
            bytes,
            &FormatOptions {
                label: Some("my disk".into()),
                ..options(Some(kind))
            },
        );
        assert_eq!(&layout.label, b"MY DISK    ");
        let image = disk.bytes();
        let label_at = if kind == FatKind::Fat32 { 71 } else { 43 };
        assert_eq!(&image[label_at..label_at + 11], b"MY DISK    ");
        let root = (layout.first_data_sector as usize - layout.root_dir_sectors as usize) * 512;
        let root = if kind == FatKind::Fat32 {
            layout.first_data_sector as usize * 512
        } else {
            root
        };
        assert_eq!(&image[root..root + 11], b"MY DISK    ");
        assert_eq!(image[root + 11], 0x08);
        let volume = Volume::open(disk).unwrap();
        assert!(volume.root_directory().unwrap().is_empty());
    }
}

#[test]
fn a_volume_inside_a_larger_device_stays_inside_it() {
    let offset = 64 * 1024u64;
    let size = 20 * MIB;
    let guard = 4096usize;
    let disk = Disk::new(vec![0xEE; offset as usize + size as usize + guard]);
    let layout = format(
        &disk,
        &FormatOptions {
            offset,
            size: Some(size),
            ..options(Some(FatKind::Fat16))
        },
    )
    .unwrap();
    assert_eq!(layout.hidden_sectors, 128);
    let image = disk.bytes();
    assert!(image[..offset as usize].iter().all(|b| *b == 0xEE));
    assert!(image[(offset + size) as usize..].iter().all(|b| *b == 0xEE));
    let volume = open_device(disk.clone(), offset).unwrap();
    assert_eq!(volume.boot().kind, FatKind::Fat16);
    assert_eq!(volume.size_bytes(), layout.volume_bytes());
}

#[test]
fn refusals_say_why_and_write_nothing() {
    let disk = Disk::new(vec![0xEE; (8 * MIB) as usize]);
    let refuse = |options: FormatOptions, why: &str| {
        let error = format(&disk, &options).unwrap_err();
        assert!(error.to_string().contains(why), "{why}: {error}");
        assert_eq!(disk.writes(), 0, "{why}: nothing written");
    };
    let base = options(None);
    refuse(
        FormatOptions {
            bytes_per_sector: 300,
            ..base.clone()
        },
        "bytes per sector",
    );
    refuse(
        FormatOptions {
            sectors_per_cluster: Some(3),
            ..base.clone()
        },
        "power of two",
    );
    refuse(
        FormatOptions {
            sectors_per_cluster: Some(128),
            bytes_per_sector: 1024,
            ..base.clone()
        },
        "64 KiB",
    );
    refuse(
        FormatOptions {
            fat_count: 0,
            ..base.clone()
        },
        "FAT copies",
    );
    refuse(
        FormatOptions {
            media: 0x12,
            ..base.clone()
        },
        "media",
    );
    refuse(
        FormatOptions {
            root_entries: Some(100),
            ..base.clone()
        },
        "whole sectors",
    );
    refuse(
        FormatOptions {
            kind: Some(FatKind::Fat32),
            root_entries: Some(512),
            ..base.clone()
        },
        "no fixed root",
    );
    refuse(
        FormatOptions {
            kind: Some(FatKind::Fat32),
            ..base.clone()
        },
        "too small",
    );
    refuse(
        FormatOptions {
            label: Some("way too long a label".into()),
            ..base.clone()
        },
        "label",
    );
    refuse(
        FormatOptions {
            label: Some("bad*name".into()),
            ..base.clone()
        },
        "label",
    );
    refuse(
        FormatOptions {
            offset: 100,
            ..base.clone()
        },
        "multiple of the sector",
    );
    refuse(
        FormatOptions {
            offset: 9 * MIB,
            ..base.clone()
        },
        "beyond the device",
    );
    refuse(
        FormatOptions {
            size: Some(9 * MIB),
            ..base.clone()
        },
        "larger than the device",
    );
    refuse(
        FormatOptions {
            size: Some(2048),
            ..base.clone()
        },
        "too small",
    );
    disk.set_writable(false);
    let error = format(&disk, &base).unwrap_err();
    assert!(matches!(error, Error::Device(fs_core::Error::ReadOnly)));
}

#[test]
fn other_sector_sizes_and_fat_counts_read_back() {
    for (bps, kind, bytes) in [
        (1024u16, FatKind::Fat16, 20 * MIB),
        (4096, FatKind::Fat16, 64 * MIB),
        (4096, FatKind::Fat32, 600 * MIB),
        (2048, FatKind::Fat12, 4 * MIB),
    ] {
        for fats in [1u8, 2] {
            let (disk, layout) = formatted(
                bytes,
                &FormatOptions {
                    bytes_per_sector: bps,
                    fat_count: fats,
                    ..options(Some(kind))
                },
            );
            let volume = Volume::open(disk).unwrap();
            assert_eq!(volume.boot().kind, kind, "{bps} {kind:?} {fats}");
            assert_eq!(volume.boot().fat_count, fats);
            assert_eq!(volume.boot().bytes_per_sector, bps);
            assert_eq!(volume.boot().cluster_count, layout.cluster_count);
        }
    }
}

#[test]
fn format_path_makes_and_grows_an_image_file() {
    let path = std::env::temp_dir().join(format!("fat-mkfs-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let error = format_path(&path, &options(None)).unwrap_err();
    assert!(error.to_string().contains("needs a size"), "{error}");
    assert!(!path.exists());
    let layout = format_path(
        &path,
        &FormatOptions {
            size: Some(2 * MIB),
            ..options(None)
        },
    )
    .unwrap();
    assert_eq!(layout.kind, FatKind::Fat12);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 2 * MIB);
    let volume = open_path(&path, 0).unwrap();
    assert_eq!(volume.boot().kind, FatKind::Fat12);
    assert!(volume.root_directory().unwrap().is_empty());
    drop(volume);
    // An existing, longer file is formatted in place and not shrunk.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(3 * MIB)
        .unwrap();
    format_path(&path, &options(Some(FatKind::Fat16))).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 3 * MIB);
    assert_eq!(open_path(&path, 0).unwrap().boot().kind, FatKind::Fat16);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn the_serial_defaults_to_something_from_the_clock() {
    let layout = plan(8 * MIB, &FormatOptions::default()).unwrap();
    assert_ne!(layout.serial, 0);
}
