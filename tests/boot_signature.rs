//! A boot sector without its `0x55 0xAA` signature: read leniently, written
//! and formatted strictly.
//!
//! `fsck.fat` and the Linux kernel's vfat driver accept a volume whose BPB is
//! intact and whose signature is zero (`tests/independent_validation.rs`
//! checks that against the real tools). The readers here do the same and
//! report it as a warning; a writer refuses the volume until it is repaired,
//! and the formatter never produces one.

mod support;

use fs_core::FileDevice;
use fs_fat32::file::FileAPI;
use fs_fat32::mkfs::{format, FormatOptions};
use fs_fat32::volume::{open_device, open_path, open_raw_device, VolumeAPI};
use fs_fat32::write::{open_raw_writable, open_writable, WriteVolume};
use fs_fat32::{BootWarning, Error, FatKind, Volume};
use support::{image, pattern, Disk};

const KINDS: [FatKind; 3] = [FatKind::Fat12, FatKind::Fat16, FatKind::Fat32];

/// A volume with one file, and the same volume with its signature replaced.
fn with_file(kind: FatKind, signature: [u8; 2]) -> (Vec<u8>, Vec<u8>) {
    let (mut bytes, layout) = image(kind);
    layout.add_root_file(&mut bytes, 0, b"HELLO   TXT", 4, &pattern(700, 3));
    let mut damaged = bytes.clone();
    damaged[510..512].copy_from_slice(&signature);
    (bytes, damaged)
}

fn read_hello<D: fs_core::BlockRead>(volume: &Volume<D>) -> Vec<u8> {
    let entries = volume.root_directory().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "HELLO.TXT");
    let file = volume.open_file(&entries[0]).unwrap();
    let mut data = vec![0; file.len() as usize];
    assert_eq!(file.read_at(0, &mut data).unwrap(), data.len());
    data
}

#[test]
fn a_missing_signature_opens_for_reading_lists_and_reads_like_the_signed_volume() {
    for kind in KINDS {
        for signature in [[0, 0], [0x55, 0], [0, 0xAA], [0xAA, 0x55]] {
            let (signed, damaged) = with_file(kind, signature);
            let disk = Disk::new(damaged.clone());

            let volume = open_device(disk.clone(), 0)
                .unwrap_or_else(|error| panic!("{kind:?} {signature:02X?}: {error}"));
            assert_eq!(volume.boot().kind, kind);
            assert!(!volume.boot().signature_ok);
            assert_eq!(volume.boot().signature, signature);
            assert_eq!(
                volume.warnings(),
                vec![BootWarning::MissingSignature { found: signature }]
            );
            assert_eq!(read_hello(&volume), pattern(700, 3));

            // Everything the geometry says matches the signed volume.
            let reference = open_device(Disk::new(signed), 0).unwrap();
            assert!(reference.warnings().is_empty());
            assert_eq!(volume.boot().cluster_count, reference.boot().cluster_count);
            assert_eq!(
                volume.boot().first_data_sector,
                reference.boot().first_data_sector
            );

            // Reading changed nothing and asked for nothing to change.
            assert_eq!(disk.bytes(), damaged, "{kind:?}: the image changed");
            assert_eq!(disk.mutations(), 0, "{kind:?}: a write was attempted");
        }
    }
}

#[test]
fn the_other_ways_in_open_it_too_and_the_raw_layer_is_unchanged() {
    for kind in KINDS {
        let (_, damaged) = with_file(kind, [0, 0]);
        let path =
            std::env::temp_dir().join(format!("fat-nosig-{}-{kind:?}.img", std::process::id()));
        std::fs::write(&path, &damaged).unwrap();
        let by_path = open_path(&path, 0).unwrap();
        assert_eq!(read_hello(&by_path), pattern(700, 3));
        drop(by_path);
        let raw = open_raw_device(FileDevice::open(&path).unwrap(), 0).unwrap();
        let mut sector = vec![0u8; 512];
        raw.read_volume_bytes(0, &mut sector).unwrap();
        assert_eq!(sector, damaged[..512]);
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
        std::fs::remove_file(&path).unwrap();

        // At a partition offset.
        let mut shifted = vec![0xAA; 4096];
        shifted.extend_from_slice(&damaged);
        let at_offset = open_device(Disk::new(shifted), 4096).unwrap();
        assert_eq!(read_hello(&at_offset), pattern(700, 3));
    }
}

#[test]
fn a_signed_volume_has_nothing_to_report() {
    for kind in KINDS {
        let (signed, _) = with_file(kind, [0, 0]);
        let volume = open_device(Disk::new(signed), 0).unwrap();
        assert!(volume.boot().signature_ok);
        assert_eq!(volume.boot().signature, [0x55, 0xAA]);
        assert!(volume.warnings().is_empty());
    }
}

#[test]
fn without_a_signature_the_geometry_still_has_to_hold() {
    type Damage = Box<dyn Fn(&mut [u8])>;
    for kind in KINDS {
        let damages: Vec<(&str, Damage)> = vec![
            ("bytes per sector 0", Box::new(|b| b[11..13].fill(0))),
            (
                "bytes per sector 300",
                Box::new(|b| b[11..13].copy_from_slice(&300u16.to_le_bytes())),
            ),
            ("sectors per cluster 3", Box::new(|b| b[13] = 3)),
            ("sectors per cluster 0", Box::new(|b| b[13] = 0)),
            ("reserved sectors 0", Box::new(|b| b[14..16].fill(0))),
            ("no FAT", Box::new(|b| b[16] = 0)),
            ("media byte 0", Box::new(|b| b[21] = 0)),
            ("media byte 0x42", Box::new(|b| b[21] = 0x42)),
            (
                "FAT far too small",
                Box::new(move |b| {
                    if kind == FatKind::Fat32 {
                        b[36..40].copy_from_slice(&1u32.to_le_bytes());
                    } else {
                        b[22..24].copy_from_slice(&1u16.to_le_bytes());
                    }
                }),
            ),
            (
                "volume longer than the device",
                Box::new(move |b| {
                    if kind == FatKind::Fat32 {
                        b[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
                    } else {
                        b[19..21].copy_from_slice(&u16::MAX.to_le_bytes());
                    }
                }),
            ),
            ("sector zeroed", Box::new(|b| b[..512].fill(0))),
            ("sector 0xFF", Box::new(|b| b[..512].fill(0xFF))),
        ];
        for (what, damage) in damages {
            let (_, mut bytes) = with_file(kind, [0, 0]);
            damage(&mut bytes);
            let disk = Disk::new(bytes.clone());
            let refused = open_device(disk.clone(), 0);
            assert!(refused.is_err(), "{kind:?}: {what} must still be refused");
            assert_eq!(disk.bytes(), bytes);
            assert_eq!(disk.mutations(), 0);
        }
    }
}

#[test]
fn an_all_zero_sector_is_refused_for_the_reason_it_always_was() {
    let disk = Disk::new(vec![0; 64 * 512]);
    let error = open_device(disk, 0).err().expect("refused");
    assert!(error.to_string().contains("missing boot-sector signature"));
}

#[test]
fn the_writable_constructors_refuse_a_missing_signature_and_say_how_to_repair() {
    for kind in KINDS {
        let (_, damaged) = with_file(kind, [0, 0]);

        let disk = Disk::new(damaged.clone());
        let error = WriteVolume::mount(disk.clone()).err().expect("refused");
        let text = error.to_string();
        assert!(matches!(error, Error::Invalid(_)), "{kind:?}: {text}");
        for needed in ["0x55 0xAA", "signature", "repair"] {
            assert!(text.contains(needed), "{kind:?}: `{needed}` not in: {text}");
        }
        // Refused before anything was written, flushed or resized.
        assert_eq!(disk.mutations(), 0, "{kind:?}");
        assert_eq!(disk.bytes(), damaged, "{kind:?}");

        let disk = Disk::new(damaged.clone());
        assert!(open_writable(disk.clone(), 0).is_err());
        assert_eq!(disk.mutations(), 0, "{kind:?}");
        assert_eq!(disk.bytes(), damaged, "{kind:?}");

        // The raw writer reads no boot sector and keeps working, so a tool
        // can make the repair the message asks for.
        let disk = Disk::new(damaged.clone());
        let raw = open_raw_writable(disk.clone(), 0).unwrap();
        raw.write_volume_bytes(510, &[0x55, 0xAA]).unwrap();
        raw.flush().unwrap();
        let mounted = WriteVolume::mount(disk.clone()).unwrap();
        assert!(mounted.boot().signature_ok);
        assert!(mounted.warnings().is_empty());
        mounted.unmount().unwrap();
        // Repairing the signature changed the signature and nothing else.
        let mut expected = damaged.clone();
        expected[510..512].copy_from_slice(&[0x55, 0xAA]);
        assert_eq!(disk.bytes(), expected, "{kind:?}");
    }
}

#[test]
fn the_formatter_always_writes_the_signature_where_it_belongs() {
    const MIB: u64 = 1024 * 1024;
    let cases = [
        (FatKind::Fat12, 512u16, 2 * MIB),
        (FatKind::Fat16, 512, 20 * MIB),
        (FatKind::Fat32, 512, 70 * MIB),
        (FatKind::Fat12, 4096, 16 * MIB),
        (FatKind::Fat16, 1024, 40 * MIB),
        (FatKind::Fat32, 1024, 80 * MIB),
    ];
    for (kind, sector, bytes) in cases {
        // Start from zeros so a missing write would show, and again from
        // ones so one that wrote the wrong thing would.
        for fill in [0x00u8, 0xEE] {
            let disk = Disk::new(vec![fill; bytes as usize]);
            let options = FormatOptions {
                kind: Some(kind),
                bytes_per_sector: sector,
                ..FormatOptions::default()
            };
            let layout = format(&disk, &options).unwrap();
            let image = disk.bytes();
            let at =
                |sector_number: usize| &image[sector_number * sector as usize..][..sector as usize];

            let mut sectors = vec![0usize];
            if kind == FatKind::Fat32 {
                // The backup boot sector, and the two boot-code sectors
                // beside each copy.
                sectors.extend([2, 6, 8]);
            }
            for number in sectors {
                let bytes = at(number);
                assert_eq!(
                    &bytes[510..512],
                    &[0x55, 0xAA],
                    "{kind:?}/{sector}: signature at 510 of sector {number}"
                );
                assert_eq!(
                    &bytes[sector as usize - 2..],
                    &[0x55, 0xAA],
                    "{kind:?}/{sector}: signature ends sector {number}"
                );
            }
            if kind == FatKind::Fat32 {
                assert_eq!(at(0), at(6), "{kind:?}/{sector}: the backup is a copy");
                assert_eq!(layout.reserved_sectors, 32);
            }

            let volume = Volume::open(Disk::new(image.clone())).unwrap();
            assert!(volume.boot().signature_ok, "{kind:?}/{sector}");
            assert!(volume.warnings().is_empty(), "{kind:?}/{sector}");
            // Mounting for writing accepts what the formatter made.
            WriteVolume::mount(Disk::new(image)).unwrap();
        }
    }
}
