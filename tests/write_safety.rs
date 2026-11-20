//! The write capability: refused unless asked for, bounded, flagged dirty
//! while it changes things and clean when it closes.
// This file still uses the superseded `file`, `primitives` and `volume` traits,
// which stay one release as deprecated wrappers; it moves to `fs_core::layers` then.
#![allow(deprecated)]

mod support;

use fs_core::BlockRead;
use fs_fat32::volume::VolumeAPI;
use fs_fat32::write::{open_raw_writable, open_writable, RawWriteVolume, WriteVolume};
use fs_fat32::{Error, FatKind};
use std::panic::{catch_unwind, AssertUnwindSafe};
use support::{image, Disk, Event, Layout};

const KINDS: [FatKind; 3] = [FatKind::Fat12, FatKind::Fat16, FatKind::Fat32];

fn state_offset(kind: FatKind) -> usize {
    if kind == FatKind::Fat32 {
        65
    } else {
        37
    }
}

fn dirty_flags(bytes: &[u8], layout: &Layout) -> (bool, Vec<bool>) {
    // The boot state byte, and FAT entry 1's clean bit in each FAT copy.
    let boot = bytes[state_offset(layout.kind)] & 1 != 0;
    let fats = (0..layout.fats)
        .map(|copy| {
            let at = layout.fat_offset(copy);
            match layout.kind {
                FatKind::Fat12 => false,
                FatKind::Fat16 => bytes[at + 3] & 0x80 == 0,
                FatKind::Fat32 => bytes[at + 7] & 0x08 == 0,
            }
        })
        .collect();
    (boot, fats)
}

#[test]
fn a_read_only_device_cannot_be_mounted_for_writing() {
    for kind in KINDS {
        let (bytes, _) = image(kind);
        let disk = Disk::new(bytes.clone());
        disk.set_writable(false);
        let error = WriteVolume::mount(disk.clone()).err().expect("refused");
        assert!(
            matches!(error, Error::Device(fs_core::Error::ReadOnly)),
            "{kind:?}: {error}"
        );
        assert_eq!(disk.writes(), 0);
        assert_eq!(disk.bytes(), bytes);
    }
}

#[test]
fn mounting_checks_the_geometry_like_the_reader_does() {
    let (mut bytes, _) = image(FatKind::Fat16);
    bytes[510] = 0;
    let disk = Disk::new(bytes);
    assert!(matches!(
        WriteVolume::mount(disk.clone()).err().unwrap(),
        Error::Invalid(_)
    ));
    assert_eq!(disk.writes(), 0);

    let (mut bytes, _) = image(FatKind::Fat16);
    bytes[11..13].copy_from_slice(&0u16.to_le_bytes());
    assert!(WriteVolume::mount(Disk::new(bytes)).is_err());

    // A volume longer than its device.
    let (bytes, layout) = image(FatKind::Fat16);
    let short = bytes[..(layout.total_sectors - 1) * 512].to_vec();
    assert!(WriteVolume::mount(Disk::new(short)).is_err());
}

#[test]
fn a_volume_marked_dirty_is_refused_and_left_alone() {
    for kind in [FatKind::Fat16, FatKind::Fat32] {
        let (clean, layout) = image(kind);
        let cases: [(&str, support::Damage); 4] = [
            ("boot dirty", Box::new(move |b| b[state_offset(kind)] |= 1)),
            ("boot error", Box::new(move |b| b[state_offset(kind)] |= 2)),
            (
                "entry 1 not clean",
                Box::new(move |b| {
                    let at = layout.fat_offset(0);
                    if kind == FatKind::Fat16 {
                        b[at + 3] &= !0x80;
                    } else {
                        b[at + 7] &= !0x08;
                    }
                }),
            ),
            (
                "entry 1 hard error",
                Box::new(move |b| {
                    let at = layout.fat_offset(0);
                    if kind == FatKind::Fat16 {
                        b[at + 3] &= !0x40;
                    } else {
                        b[at + 7] &= !0x04;
                    }
                }),
            ),
        ];
        for (what, damage) in cases {
            let mut bytes = clean.clone();
            damage(&mut bytes);
            let disk = Disk::new(bytes.clone());
            let error = WriteVolume::mount(disk.clone()).err().expect(what);
            assert!(matches!(error, Error::Dirty(_)), "{kind:?} {what}: {error}");
            assert_eq!(disk.writes(), 0, "{kind:?} {what}");
            assert_eq!(disk.bytes(), bytes, "{kind:?} {what}");
        }
    }
}

#[test]
fn fat12_has_no_dirty_flag_to_check() {
    let (bytes, _) = image(FatKind::Fat12);
    assert!(WriteVolume::mount(Disk::new(bytes)).is_ok());
}

#[test]
fn mounting_and_closing_without_a_change_writes_nothing() {
    for kind in KINDS {
        let (bytes, _) = image(kind);
        let disk = Disk::new(bytes.clone());
        let volume = WriteVolume::mount(disk.clone()).unwrap();
        assert!(!volume.is_marked_dirty());
        volume.unmount().unwrap();
        assert_eq!(disk.writes(), 0, "{kind:?}");
        assert_eq!(disk.bytes(), bytes, "{kind:?}");
    }
}

#[test]
fn the_dirty_flags_are_set_and_flushed_before_the_first_change_and_cleared_last() {
    for kind in [FatKind::Fat16, FatKind::Fat32] {
        let (bytes, layout) = image(kind);
        let disk = Disk::new(bytes.clone());
        let mut volume = WriteVolume::mount(disk.clone()).unwrap();
        volume.write_fat_entry(5, 0).unwrap();
        assert!(volume.is_marked_dirty());
        let (boot, fats) = dirty_flags(&disk.bytes(), &layout);
        assert!(boot, "{kind:?}: boot state is dirty while mutating");
        assert!(fats.iter().all(|d| *d), "{kind:?}: every FAT copy is dirty");

        // The boot sector, then each FAT copy's entry 1, then a flush, and only
        // then the change itself in each copy.
        let write = |offset: usize| Event::Write {
            offset: offset as u64,
            length: 512,
        };
        assert_eq!(
            disk.events(),
            vec![
                write(0),
                write(layout.fat_offset(0)),
                write(layout.fat_offset(1)),
                Event::Flush,
                write(layout.fat_offset(0)),
                write(layout.fat_offset(1)),
            ],
            "{kind:?}"
        );

        volume.unmount().unwrap();
        let after = disk.bytes();
        assert_eq!(dirty_flags(&after, &layout), (false, vec![false; 2]));
        // Entry 5 was already free, so the volume is byte for byte as it was.
        assert_eq!(after, bytes, "{kind:?}: closing restores the flags");
        assert_eq!(disk.events().last(), Some(&Event::Flush));
    }
}

#[test]
fn a_crash_leaves_the_volume_dirty_and_the_next_writer_refuses_it() {
    for kind in [FatKind::Fat16, FatKind::Fat32] {
        let (bytes, layout) = image(kind);
        let disk = Disk::new(bytes);
        let mut volume = WriteVolume::mount(disk.clone()).unwrap();
        volume.write_fat_entry(7, 0xFFF).unwrap();
        let crashed = disk.bytes(); // the disk as a power cut leaves it
        drop(volume);
        let (boot, fats) = dirty_flags(&crashed, &layout);
        assert!(boot && fats.iter().all(|d| *d), "{kind:?}");
        assert!(matches!(
            WriteVolume::mount(Disk::new(crashed)).err().unwrap(),
            Error::Dirty(_)
        ));
    }
}

#[test]
fn dropping_a_mount_closes_it_cleanly_and_a_panic_leaves_it_dirty() {
    let (bytes, layout) = image(FatKind::Fat16);
    let disk = Disk::new(bytes);
    {
        let mut volume = WriteVolume::mount(disk.clone()).unwrap();
        volume.write_fat_entry(9, 0xFFFF).unwrap();
    }
    assert_eq!(dirty_flags(&disk.bytes(), &layout), (false, vec![false; 2]));

    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut volume = WriteVolume::mount(disk.clone()).unwrap();
        volume.write_fat_entry(10, 0xFFFF).unwrap();
        panic!("stopped partway");
    }));
    assert!(result.is_err());
    let (boot, fats) = dirty_flags(&disk.bytes(), &layout);
    assert!(boot && fats.iter().all(|d| *d));
}

#[test]
fn a_write_that_fails_partway_leaves_the_mount_failed_and_the_volume_dirty() {
    let (bytes, layout) = image(FatKind::Fat32);
    let disk = Disk::new(bytes);
    let mut volume = WriteVolume::mount(disk.clone()).unwrap();
    volume.write_fat_entry(5, 0).unwrap();
    // The next FAT write reaches the first copy and fails on the second.
    disk.fail_writes_after(1);
    let error = volume.write_fat_entry(6, 0x0FFF_FFFF).unwrap_err();
    assert!(matches!(error, Error::Device(_)), "{error}");
    disk.fail_writes_after(u64::MAX);
    assert!(matches!(volume.write_fat_entry(7, 0), Err(Error::Failed)));
    assert!(matches!(
        volume.write_cluster(3, 0, &[1]),
        Err(Error::Failed)
    ));
    assert!(matches!(volume.flush(), Err(Error::Failed)));
    assert!(matches!(volume.unmount(), Err(Error::Failed)));
    let (boot, fats) = dirty_flags(&disk.bytes(), &layout);
    assert!(boot && fats.iter().all(|d| *d), "left marked dirty");
}

#[test]
fn a_refused_change_that_wrote_nothing_does_not_fail_the_mount() {
    let (bytes, layout) = image(FatKind::Fat16);
    let disk = Disk::new(bytes);
    let mut volume = WriteVolume::mount(disk.clone()).unwrap();
    let past_end = layout.clusters as u32 + 2;
    assert!(matches!(
        volume.write_fat_entry(past_end, 0),
        Err(Error::Bounds)
    ));
    assert!(matches!(
        volume.write_fat_entry(5, 0x1_0000),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        volume.write_cluster(2, 510, &[0; 3]),
        Err(Error::Bounds)
    ));
    assert!(matches!(
        volume.write_cluster(1, 0, &[0]),
        Err(Error::Bounds)
    ));
    assert!(matches!(
        volume.write_cluster(past_end, 0, &[0]),
        Err(Error::Bounds)
    ));
    assert_eq!(
        disk.writes(),
        0,
        "validation fails before anything is written"
    );
    volume.write_fat_entry(5, 0).unwrap();
    volume.unmount().unwrap();
}

#[test]
fn writes_stay_inside_the_volume_inside_a_bigger_device() {
    let (volume_bytes, layout) = image(FatKind::Fat16);
    let offset = 2048usize;
    let guard = 4096usize;
    let mut bytes = vec![0xEE; offset];
    bytes.extend_from_slice(&volume_bytes);
    bytes.extend(vec![0xEE; guard]);
    let disk = Disk::new(bytes.clone());
    let mut volume = open_writable(disk.clone(), offset as u64).unwrap();
    assert_eq!(volume.size_bytes(), volume_bytes.len() as u64);
    let last = layout.clusters as u32 + 1;
    volume.write_cluster(last, 0, &[0xAB; 512]).unwrap();
    assert!(matches!(
        volume.write_cluster(last + 1, 0, &[1]),
        Err(Error::Bounds)
    ));
    volume.write_fat_entry(last, 0xFFFF).unwrap();
    volume.unmount().unwrap();
    let after = disk.bytes();
    assert!(
        after[..offset].iter().all(|b| *b == 0xEE),
        "before the volume"
    );
    assert!(
        after[offset + volume_bytes.len()..]
            .iter()
            .all(|b| *b == 0xEE),
        "after the volume"
    );
    let at = offset + layout.cluster_offset(last as usize);
    assert_eq!(&after[at..at + 512], &[0xAB; 512]);

    assert!(open_writable(disk.clone(), 100).is_err(), "misaligned");
    assert!(open_writable(disk.clone(), bytes.len() as u64).is_err());
}

#[test]
fn a_fat12_entry_across_a_sector_boundary_keeps_its_neighbours() {
    let (bytes, layout) = image(FatKind::Fat12);
    let disk = Disk::new(bytes);
    let mut volume = WriteVolume::mount(disk.clone()).unwrap();
    // Cluster 341 starts at FAT byte 511: its 12 bits straddle two sectors.
    for (cluster, value) in [(340u32, 0xABC), (341, 0x123), (342, 0xFED), (343, 0x456)] {
        volume.write_fat_entry(cluster, value).unwrap();
    }
    for (cluster, value) in [(340u32, 0xABC), (341, 0x123), (342, 0xFED), (343, 0x456)] {
        assert_eq!(volume.fat_entry_raw(cluster).unwrap(), value, "{cluster}");
    }
    // Overwriting one leaves the entry that shares its byte alone.
    volume.write_fat_entry(341, 0xFFF).unwrap();
    assert_eq!(volume.fat_entry_raw(340).unwrap(), 0xABC);
    assert_eq!(volume.fat_entry_raw(342).unwrap(), 0xFED);
    volume.unmount().unwrap();
    for copy in 0..2 {
        let fat = &disk.bytes()[layout.fat_offset(copy)..][..12 * 512];
        assert_eq!(&fat[510..513], &[0xBC, 0xFA, 0xFF], "copy {copy}");
    }
}

#[test]
fn every_fat_copy_is_kept_in_step_and_fat32_keeps_its_reserved_bits() {
    for kind in KINDS {
        let (mut bytes, layout) = image(kind);
        if kind == FatKind::Fat32 {
            // Reserved top nibble of entry 9, in both copies.
            for copy in 0..2 {
                let at = layout.fat_offset(copy) + 9 * 4 + 3;
                bytes[at] = 0xA0;
            }
        }
        let disk = Disk::new(bytes);
        let mut volume = WriteVolume::mount(disk.clone()).unwrap();
        volume.write_fat_entry(9, 0x123).unwrap();
        volume.write_fat_entry(10, 0x456).unwrap();
        volume.unmount().unwrap();
        let after = disk.bytes();
        let fat_bytes = layout.fat_sectors * 512;
        assert_eq!(
            after[layout.fat_offset(0)..][..fat_bytes],
            after[layout.fat_offset(1)..][..fat_bytes],
            "{kind:?}: copies agree"
        );
        if kind == FatKind::Fat32 {
            let at = layout.fat_offset(1) + 9 * 4;
            assert_eq!(after[at + 3], 0xA0, "reserved bits kept");
            assert_eq!(after[at], 0x23);
        }
    }
}

#[test]
fn a_fat32_volume_with_mirroring_off_writes_only_its_active_copy() {
    let (mut bytes, layout) = image(FatKind::Fat32);
    bytes[40] = 0x80 | 1; // mirroring disabled, FAT 1 active
    let disk = Disk::new(bytes.clone());
    let mut volume = WriteVolume::mount(disk.clone()).unwrap();
    volume.write_fat_entry(20, 0x0FFF_FFFF).unwrap();
    volume.unmount().unwrap();
    let after = disk.bytes();
    let entry = |copy: usize| {
        let at = layout.fat_offset(copy) + 20 * 4;
        u32::from_le_bytes(after[at..at + 4].try_into().unwrap())
    };
    assert_eq!(entry(1), 0x0FFF_FFFF);
    assert_eq!(entry(0), 0, "the inactive copy is not touched");
}

#[test]
fn data_clusters_are_written_where_the_fat_geometry_says() {
    for kind in KINDS {
        let (bytes, layout) = image(kind);
        let disk = Disk::new(bytes);
        let mut volume = WriteVolume::mount(disk.clone()).unwrap();
        volume.write_cluster(3, 100, b"hello").unwrap();
        volume.write_cluster(2, 0, &[7u8; 512]).unwrap();
        volume.unmount().unwrap();
        let after = disk.bytes();
        let at = layout.cluster_offset(3) + 100;
        assert_eq!(&after[at..at + 5], b"hello", "{kind:?}");
        let at = layout.cluster_offset(2);
        assert_eq!(&after[at..at + 512], &[7u8; 512], "{kind:?}");
    }
}

#[test]
fn raw_writing_is_a_separate_explicit_capability_and_bounded() {
    let (bytes, _) = image(FatKind::Fat16);
    let disk = Disk::new(bytes.clone());
    let raw = RawWriteVolume::open(disk.clone()).unwrap();
    raw.write_volume_bytes(3, &[1, 2, 3]).unwrap();
    raw.flush().unwrap();
    let mut back = [0u8; 3];
    raw.read_volume_bytes(3, &mut back).unwrap();
    assert_eq!(back, [1, 2, 3]);
    // No dirty flags, no validation: the boot sector is not even read.
    assert_eq!(disk.writes(), 1);
    let end = disk.size_bytes();
    assert!(matches!(
        raw.write_volume_bytes(end - 1, &[0, 0]),
        Err(Error::Bounds)
    ));
    assert!(matches!(
        raw.write_volume_bytes(u64::MAX, &[0, 0]),
        Err(Error::Bounds)
    ));

    disk.set_writable(false);
    assert!(matches!(
        RawWriteVolume::open(disk.clone()).err().unwrap(),
        Error::Device(fs_core::Error::ReadOnly)
    ));

    disk.set_writable(true);
    let mut padded = vec![0xCC; 1024];
    padded.extend_from_slice(&bytes);
    let disk = Disk::new(padded);
    let raw = open_raw_writable(disk.clone(), 1024).unwrap();
    raw.write_volume_bytes(0, &[9]).unwrap();
    assert_eq!(disk.bytes()[1024], 9);
    assert_eq!(disk.bytes()[1023], 0xCC);
    assert!(open_raw_writable(disk, 7).is_err());
}

#[test]
fn flush_asks_the_device_to_flush() {
    let (bytes, _) = image(FatKind::Fat32);
    let disk = Disk::new(bytes);
    let mut volume = WriteVolume::mount(disk.clone()).unwrap();
    let before = disk.flushes();
    volume.flush().unwrap();
    assert_eq!(disk.flushes(), before + 1);
}
