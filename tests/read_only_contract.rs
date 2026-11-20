//! A source opened for reading is never changed.
//!
//! Recovery reads disks that must not change. Every read path of the crate
//! runs here over a device that records any attempt to write, flush or
//! resize it, with the device able to fail partway as a cancelled or failing
//! read would, and over the damage that sends the readers down their error
//! paths. The device can write; the point is that the read-only constructors
//! never ask it to.
// This file still uses the superseded `file`, `primitives` and `volume` traits,
// which stay one release as deprecated wrappers; it moves to `fs_core::layers` then.
#![allow(deprecated)]

mod support;

use fs_core::FileDevice;
use fs_fat32::directory::{parse_directory, scan_directory};
use fs_fat32::file::FileAPI;
use fs_fat32::volume::{open_device, open_path, open_raw_device, RawVolume, Volume, VolumeAPI};
use fs_fat32::FatKind;
use std::path::PathBuf;
use support::{image, pattern, Disk, Layout};

const KINDS: [FatKind; 3] = [FatKind::Fat12, FatKind::Fat16, FatKind::Fat32];

/// An image with a two-cluster file, a one-cluster file whose chain is cut
/// short of its size, and a deleted entry.
fn populated(kind: FatKind) -> (Vec<u8>, Layout) {
    let (mut bytes, layout) = image(kind);
    layout.add_root_file(&mut bytes, 0, b"TWO     BIN", 4, &pattern(700, 1));
    layout.add_root_file(&mut bytes, 1, b"SHORT   BIN", 8, &pattern(100, 2));
    // Claims three clusters but the chain stops after one.
    let root = match kind {
        FatKind::Fat32 => layout.cluster_offset(2),
        _ => layout.root_offset(),
    };
    bytes[root + 64..root + 75].copy_from_slice(b"CUT     BIN");
    bytes[root + 75] = 0x20;
    bytes[root + 64 + 26..root + 64 + 28].copy_from_slice(&12u16.to_le_bytes());
    bytes[root + 64 + 28..root + 64 + 32].copy_from_slice(&1500u32.to_le_bytes());
    layout.set_entry(&mut bytes, 12, 0xFFF);
    bytes[root + 96..root + 107].copy_from_slice(b"\xE5ELETED BIN");
    bytes[root + 107] = 0x20;
    (bytes, layout)
}

/// Every read the crate offers, ignoring the errors (they are part of the
/// point), until the device stops answering.
fn read_everything<Device: fs_core::BlockRead>(volume: &Volume<Device>) {
    let boot = volume.boot().clone();
    let mut sector = vec![0u8; 512];
    let _ = volume.read_volume_bytes(0, &mut sector);
    let _ = volume.read_volume_bytes(volume.size_bytes(), &mut sector);
    let _ = volume.read_volume_bytes(u64::MAX, &mut sector);
    let mut cluster = vec![0u8; boot.cluster_size() as usize];
    for number in [0, 1, 2, 3, boot.cluster_count + 1, boot.cluster_count + 2] {
        let _ = volume.read_cluster(number, &mut cluster);
    }
    let mut wrong = vec![0u8; 7];
    let _ = volume.read_cluster(2, &mut wrong);
    for number in 0..40 {
        let _ = volume.fat_entry_raw(number);
        let _ = volume.fat_value(number);
        let _ = volume.chain_candidates(number, 16);
    }
    let _ = volume.fat_entry_raw(boot.cluster_count + 5);
    if let Ok(entries) = volume.root_directory() {
        for entry in &entries {
            if let Ok(file) = volume.open_file(entry) {
                let mut data = vec![0u8; entry.size as usize + 10];
                let _ = file.read_at(0, &mut data);
                let _ = file.read_at(u64::MAX, &mut data);
                let _ = file.read_at(3, &mut data[..5]);
            }
        }
    }
    let mut raw = vec![0u8; 4096];
    let _ = volume.read_volume_bytes(0, &mut raw);
    for kind in KINDS {
        let _ = parse_directory(&raw, kind);
        let _ = scan_directory(&raw, kind);
    }
    let _ = parse_directory(&raw[..33], FatKind::Fat16);
}

fn assert_untouched(disk: &Disk, original: &[u8], what: &str) {
    assert_eq!(disk.writes(), 0, "{what}: write_at was called");
    assert_eq!(disk.flushes(), 0, "{what}: flush was called");
    assert_eq!(disk.set_lens(), 0, "{what}: set_len was called");
    assert_eq!(disk.bytes(), original, "{what}: the bytes changed");
}

#[test]
fn reading_with_every_read_only_constructor_never_touches_the_device() {
    for kind in KINDS {
        let (bytes, _) = populated(kind);
        let disk = Disk::new(bytes.clone());

        let volume = open_device(disk.clone(), 0).unwrap();
        read_everything(&volume);
        assert_untouched(&disk, &bytes, "open_device");

        let volume = Volume::open(disk.clone()).unwrap();
        read_everything(&volume);
        assert_untouched(&disk, &bytes, "Volume::open");

        let raw = open_raw_device(disk.clone(), 0).unwrap();
        let mut buffer = vec![0u8; 512];
        raw.read_volume_bytes(0, &mut buffer).unwrap();
        assert!(raw
            .read_volume_bytes(raw.size_bytes(), &mut buffer)
            .is_err());
        assert_untouched(&disk, &bytes, "open_raw_device");

        let raw = RawVolume::open(disk.clone());
        raw.read_volume_bytes(510, &mut buffer[..2]).unwrap();
        assert_untouched(&disk, &bytes, "RawVolume::open");
    }
}

#[test]
fn a_partition_offset_reads_without_writing() {
    let (volume, _) = populated(FatKind::Fat16);
    let mut bytes = vec![0xAA; 4096];
    bytes.extend_from_slice(&volume);
    let disk = Disk::new(bytes.clone());
    let opened = open_device(disk.clone(), 4096).unwrap();
    read_everything(&opened);
    assert!(open_device(disk.clone(), 100).is_err());
    assert!(open_raw_device(disk.clone(), bytes.len() as u64).is_err());
    assert_untouched(&disk, &bytes, "offset");
}

#[test]
fn damage_sends_the_readers_down_their_error_paths_without_writing() {
    for kind in KINDS {
        let (clean, layout) = populated(kind);
        let damages: [(&str, support::Damage); 8] = [
            ("no boot signature", Box::new(|b| b[510] = 0)),
            ("zeroed boot sector", Box::new(|b| b[..512].fill(0))),
            ("zero bytes per sector", Box::new(|b| b[11..13].fill(0))),
            ("zero sectors per cluster", Box::new(|b| b[13] = 0)),
            ("zero FAT size", Box::new(|b| b[16] = 0)),
            ("FAT loop", Box::new(move |b| layout.set_entry(b, 4, 4))),
            (
                "FAT points outside",
                Box::new(move |b| layout.set_entry(b, 4, 0xFF0)),
            ),
            (
                "directory garbage",
                Box::new(move |b| {
                    let root = match layout.kind {
                        FatKind::Fat32 => layout.cluster_offset(2),
                        _ => layout.root_offset(),
                    };
                    b[root..root + 512].fill(0xFF);
                }),
            ),
        ];
        for (what, damage) in damages {
            let mut bytes = clean.clone();
            damage(&mut bytes);
            let disk = Disk::new(bytes.clone());
            if let Ok(volume) = open_device(disk.clone(), 0) {
                read_everything(&volume);
            }
            if let Ok(raw) = open_raw_device(disk.clone(), 0) {
                let mut sector = vec![0u8; 512];
                let _ = raw.read_volume_bytes(0, &mut sector);
            }
            assert_untouched(&disk, &bytes, &format!("{kind:?} {what}"));
        }
    }
}

#[test]
fn a_read_that_stops_answering_leaves_the_source_unchanged() {
    // The device stops answering after N reads, for every N: a cancelled or
    // failing read at each point of opening and reading.
    for kind in KINDS {
        let (bytes, _) = populated(kind);
        for budget in 0..64 {
            let disk = Disk::new(bytes.clone());
            disk.fail_reads_after(budget);
            if let Ok(volume) = open_device(disk.clone(), 0) {
                read_everything(&volume);
            }
            assert_untouched(&disk, &bytes, &format!("{kind:?} after {budget} reads"));
        }
    }
}

#[test]
fn a_file_opened_by_path_hashes_the_same_after_reading() {
    for kind in KINDS {
        let (bytes, _) = populated(kind);
        let path: PathBuf =
            std::env::temp_dir().join(format!("fat-read-only-{}-{kind:?}.img", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let volume = open_path(&path, 0).unwrap();
        read_everything(&volume);
        drop(volume);
        let _ = open_path(&path, 512);
        let device = FileDevice::open(&path).unwrap();
        let raw = open_raw_device(device, 0).unwrap();
        let mut sector = vec![0u8; 512];
        raw.read_volume_bytes(0, &mut sector).unwrap();
        let after = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(after, bytes, "{kind:?}: the image changed");
    }
}

#[test]
fn a_device_that_can_write_is_not_written_through_a_reader() {
    // Writability of the supplied device does not leak through the reader: the
    // device is a BlockDevice that would accept a write, and `open_device`
    // wraps it so the volume it returns can only read.
    let (bytes, _) = populated(FatKind::Fat32);
    let disk = Disk::new(bytes.clone());
    let writable = fs_core::BlockDevice::is_writable(&disk);
    assert!(writable);
    let volume = open_device(disk.clone(), 0).unwrap();
    let device = volume.device();
    // `OwnedSlice` is the reader's device type; its BlockDevice side refuses.
    assert!(!fs_core::BlockDevice::is_writable(device));
    assert!(matches!(
        fs_core::BlockDevice::write_at(device, 0, &[1]),
        Err(fs_core::Error::ReadOnly)
    ));
    assert_untouched(&disk, &bytes, "write through the reader's device");
}
