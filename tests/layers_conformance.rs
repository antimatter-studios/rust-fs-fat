//! rust-fs-core's three-layer conformance checks (`fs_core::layers::
//! conformance`) run against FAT12, FAT16 and FAT32 volumes.
//!
//! The images are formatted by this crate's `mkfs` and filled through its
//! write path, in memory: long names, a fragmented file, a directory that
//! spans clusters, a nested tree, a deleted file. Damaged copies each leave
//! the evidence the salvage set requires of them:
//!
//! - FAT32 only, sector 0 zeroed: a `Signature` finding, opened for salvage
//!   through the backup boot sector;
//! - a file's chain cut short, and one that loops back on itself: a walk that
//!   ends before its claim;
//! - a directory sector that cannot be read: an `Unreadable` finding;
//! - a FAT copy that differs from the active one: a `Mismatch` finding;
//! - an image cut in half: a `Missing` finding.
//!
//! What the kernel, `fsck.fat` and mtools say about volumes they made is
//! checked in `tests/layers_oracle.rs`.

mod support;

use fs_core::layers::conformance::salvage::{run, Damaged, Driver, Evidence};
use fs_core::layers::conformance::{
    check_damaged, check_files_against_candidates, check_healthy, Expected, Limits,
};
use fs_core::layers::{
    CandidateKind, CandidateState, DamageKind, Extent, FileAPI, HealthState, Integrity, NodeKind,
    OpenVolume, PrimitiveAPI, SalvageAdapter, VolumeAPI,
};
use fs_core::BlockRead;
use fs_fat32::layers::FatAdapter;
use fs_fat32::mkfs::{format, FormatOptions};
use fs_fat32::write::WriteVolume;
use fs_fat32::{Error, FatKind, Volume};
use support::{pattern, Disk};

const MIB: u64 = 1024 * 1024;

/// An owned copy of an image, readable as a source.
struct Owned(Vec<u8>);

impl BlockRead for Owned {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core::Result<()> {
        let out_of_bounds = || fs_core::Error::OutOfBounds {
            offset,
            len: output.len() as u64,
            size: self.0.len() as u64,
        };
        let start = usize::try_from(offset).map_err(|_| out_of_bounds())?;
        let end = start
            .checked_add(output.len())
            .filter(|end| *end <= self.0.len())
            .ok_or_else(out_of_bounds)?;
        output.copy_from_slice(&self.0[start..end]);
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

/// A borrowed source, for the strict API, which takes its device by value.
struct Borrowed<'a>(&'a dyn BlockRead);

impl BlockRead for Borrowed<'_> {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core::Result<()> {
        self.0.read_at(offset, output)
    }

    fn size_bytes(&self) -> u64 {
        self.0.size_bytes()
    }
}

const SIZES: [(FatKind, u64); 3] = [
    (FatKind::Fat12, 2 * MIB),
    (FatKind::Fat16, 8 * MIB),
    (FatKind::Fat32, 40 * MIB),
];

/// What the images hold, for the expectations and for aiming damage.
struct Built {
    image: Vec<u8>,
    kind: FatKind,
}

fn long_name(index: usize) -> String {
    format!("a long file name number {index}.txt")
}

fn build(kind: FatKind, size: u64) -> Built {
    let disk = Disk::new(vec![0; size as usize]);
    format(
        &disk,
        &FormatOptions {
            kind: Some(kind),
            serial: Some(0x1234_ABCD),
            label: Some("LAYERS".into()),
            ..FormatOptions::default()
        },
    )
    .unwrap();
    let mut volume = WriteVolume::mount(disk.clone()).unwrap();
    let write = |volume: &mut WriteVolume<Disk>, path: &str, offset: u64, data: &[u8]| {
        let handle = match volume.open(path) {
            Ok(handle) => handle,
            Err(_) => volume.create_file(path).unwrap(),
        };
        volume.write(&handle, offset, data).unwrap();
    };
    write(&mut volume, "/hello.txt", 0, b"hello\n");
    write(&mut volume, "/Long File Name Here.txt", 0, &pattern(700, 1));
    write(&mut volume, "/empty.txt", 0, b"");
    volume.mkdir("/docs").unwrap();
    volume.mkdir("/docs/nested").unwrap();
    write(&mut volume, "/docs/readme.md", 0, &pattern(5000, 2));
    write(&mut volume, "/docs/nested/deep.bin", 0, &pattern(9000, 3));
    // A fragmented file: its second half lands after another file's clusters.
    write(&mut volume, "/frag.bin", 0, &pattern(1500, 4));
    write(&mut volume, "/pad.bin", 0, &pattern(1500, 5));
    write(&mut volume, "/frag.bin", 1500, &pattern(3000, 6));
    // A directory that spans several clusters, and long names across them.
    volume.mkdir("/big").unwrap();
    for index in 0..40 {
        let path = format!("/big/{}", long_name(index));
        write(&mut volume, &path, 0, format!("file {index}\n").as_bytes());
    }
    write(&mut volume, "/gone.txt", 0, &pattern(3000, 7));
    write(&mut volume, "/docs/gone-too.txt", 0, &pattern(1200, 8));
    volume.unlink("/gone.txt").unwrap();
    volume.unlink("/docs/gone-too.txt").unwrap();
    volume.unmount().unwrap();
    Built {
        image: disk.bytes(),
        kind,
    }
}

fn expected() -> Vec<Expected> {
    let mut files = vec![
        Expected::file("/hello.txt", b"hello\n"),
        Expected::file("/HELLO.TXT", b"hello\n"),
        Expected::file("/Long File Name Here.txt", &pattern(700, 1)),
        Expected::file("/empty.txt", b""),
        Expected::directory("/docs"),
        Expected::directory("/docs/nested"),
        Expected::file("/docs/readme.md", &pattern(5000, 2)),
        Expected::file("/docs/nested/deep.bin", &pattern(9000, 3)),
        Expected::file("/frag.bin", &[pattern(1500, 4), pattern(3000, 6)].concat()),
        Expected::file("/pad.bin", &pattern(1500, 5)),
        Expected::directory("/big"),
        Expected::absent("/gone.txt"),
        Expected::absent("/docs/gone-too.txt"),
    ];
    for index in 0..40 {
        files.push(Expected::file(
            &format!("/big/{}", long_name(index)),
            format!("file {index}\n").as_bytes(),
        ));
    }
    files
}

// ------------------------------------------------------------------- damage

/// Sets a FAT entry in every copy of the FAT.
fn set_entry(image: &mut [u8], kind: FatKind, cluster: u32, value: u32) {
    let boot = fs_fat32::BootSector::parse(&image[..512]).unwrap();
    for copy in 0..u64::from(boot.fat_count) {
        let fat = (u64::from(boot.reserved_sectors) + copy * u64::from(boot.sectors_per_fat))
            * u64::from(boot.bytes_per_sector);
        let at = fat as usize
            + match kind {
                FatKind::Fat12 => cluster as usize * 3 / 2,
                FatKind::Fat16 => cluster as usize * 2,
                FatKind::Fat32 => cluster as usize * 4,
            };
        match kind {
            FatKind::Fat12 => {
                let word = u16::from_le_bytes([image[at], image[at + 1]]);
                let word = if cluster & 1 == 0 {
                    (word & 0xF000) | (value as u16 & 0x0FFF)
                } else {
                    (word & 0x000F) | ((value as u16) << 4)
                };
                image[at..at + 2].copy_from_slice(&word.to_le_bytes());
            }
            FatKind::Fat16 => image[at..at + 2].copy_from_slice(&(value as u16).to_le_bytes()),
            FatKind::Fat32 => image[at..at + 4].copy_from_slice(&value.to_le_bytes()),
        }
    }
}

/// The chain of a file or directory as the healthy image lays it out, found
/// with the crate's own file layer.
fn chain_of(image: &[u8], path: &str) -> Vec<u32> {
    let volume = Volume::open(Owned(image.to_vec())).unwrap();
    let node = volume.lookup(path).unwrap();
    volume
        .chain_candidates(node.first_cluster(), usize::MAX)
        .unwrap()
}

fn damaged_cases(built: &Built) -> Vec<Damaged> {
    let healthy = &built.image;
    let kind = built.kind;
    let mut cases = Vec::new();

    if kind == FatKind::Fat32 {
        let mut image = healthy.clone();
        image[..512].fill(0);
        cases.push(Damaged::new(
            "boot sector zeroed",
            image,
            Evidence::Damage(DamageKind::Signature),
        ));
    }

    let deep = chain_of(healthy, "/docs/nested/deep.bin");
    assert!(deep.len() >= 3, "the file needs several clusters: {deep:?}");
    let mut cut = healthy.clone();
    set_entry(&mut cut, kind, deep[1], 0x0FFF_FFFF);
    cases.push(Damaged::new("chain cut short", cut, Evidence::BrokenWalk));

    let mut looped = healthy.clone();
    set_entry(&mut looped, kind, deep[1], deep[0]);
    cases.push(Damaged::new(
        "chain loops back",
        looped,
        Evidence::BrokenWalk,
    ));

    let big = chain_of(healthy, "/big");
    assert!(big.len() >= 2, "the directory spans clusters: {big:?}");
    let volume = Volume::open(Owned(healthy.clone())).unwrap();
    let sector_in_big = volume.cluster_offset(big[1]).unwrap();
    cases.push(
        Damaged::new(
            "directory sector unreadable",
            healthy.clone(),
            Evidence::Damage(DamageKind::Unreadable),
        )
        .unreadable(Extent::new(sector_in_big, 512)),
    );

    let boot = volume.boot();
    if boot.fat_count > 1 {
        let mut diverged = healthy.clone();
        let second_fat = (u64::from(boot.reserved_sectors) + u64::from(boot.sectors_per_fat))
            * u64::from(boot.bytes_per_sector);
        let at = second_fat as usize + 64;
        for byte in &mut diverged[at..at + 8] {
            *byte ^= 0x5A;
        }
        cases.push(Damaged::new(
            "FAT copies differ",
            diverged,
            Evidence::Damage(DamageKind::Mismatch),
        ));
    }

    let mut truncated = healthy.clone();
    truncated.truncate(healthy.len() / 2);
    cases.push(Damaged::new(
        "image cut in half",
        truncated,
        Evidence::Damage(DamageKind::Missing),
    ));
    cases
}

// ------------------------------------------------------------------- driver

struct Fat {
    built: Built,
}

impl Driver for Fat {
    type Adapter = FatAdapter;
    type StrictError = Error;

    /// The adapter a caller holds from opening the volume while it still
    /// read: a source that reads nowhere gets the healthy image's geometry.
    fn salvage(&self, source: &dyn BlockRead) -> FatAdapter {
        FatAdapter::from_source(source).unwrap_or_else(|_| {
            FatAdapter::from_source(&Owned(self.built.image.clone()))
                .expect("the healthy image has a boot sector")
        })
    }

    /// The strict API: open the volume, take its word for its own health,
    /// and read every directory and every file whole.
    fn strict(&self, source: &dyn BlockRead) -> Result<(), Error> {
        let volume = Volume::open(Borrowed(source))?;
        if volume.health()?.state != HealthState::Clean {
            return Err(Error::Invalid("the volume reports damage"));
        }
        let mut pending = vec![volume.root()?];
        while let Some(directory) = pending.pop() {
            for entry in volume.read_dir(&directory)? {
                match entry.kind {
                    NodeKind::Directory => pending.push(entry.node),
                    _ => {
                        volume.copy_to(&entry.node, &mut std::io::sink())?;
                    }
                }
            }
        }
        Ok(())
    }

    fn healthy(&self) -> Vec<u8> {
        self.built.image.clone()
    }

    fn damaged(&self) -> Vec<Damaged> {
        damaged_cases(&self.built)
    }
}

// -------------------------------------------------------------------- tests

#[test]
fn healthy_volumes_pass_every_layer_check() {
    for (kind, size) in SIZES {
        let built = build(kind, size);
        let volume = Volume::open(Owned(built.image.clone())).unwrap();
        let limits = Limits::default();
        let mut report = check_healthy(&volume, &expected(), &limits);
        report.merge(check_files_against_candidates(&volume, &limits));
        assert!(report.passed(), "{kind:?}:\n{report}");
    }
}

#[test]
fn the_salvage_set_passes_on_healthy_and_damaged_images() {
    for (kind, size) in SIZES {
        let report = run(&Fat {
            built: build(kind, size),
        });
        assert!(report.conforms(), "{kind:?}: {report}");
    }
}

#[test]
fn damaged_volumes_opened_for_salvage_pass_the_primitive_checks() {
    for (kind, size) in SIZES {
        let built = build(kind, size);
        for case in damaged_cases(&built) {
            if !case.unreadable.is_empty() {
                continue;
            }
            let volume = Volume::open_for_salvage(Owned(case.image.clone()))
                .unwrap_or_else(|e| panic!("{kind:?} {}: {e}", case.name));
            let report = check_damaged(&volume, &Limits::default());
            assert!(report.passed(), "{kind:?} {}:\n{report}", case.name);
            // The quick verdict reads the boot sectors, the FAT's head and the
            // root; a chain cut deep in the volume is `report_damage`'s.
            if !case.name.starts_with("chain") {
                let health = volume.health().unwrap();
                assert_ne!(
                    health.state,
                    HealthState::Clean,
                    "{kind:?} {}: {health:?}",
                    case.name
                );
            }
        }
    }
}

#[test]
fn format_names_and_geometry_follow_the_boot_sector() {
    for (kind, size) in SIZES {
        let built = build(kind, size);
        let volume = Volume::open(Owned(built.image.clone())).unwrap();
        let name = match kind {
            FatKind::Fat12 => "fat12",
            FatKind::Fat16 => "fat16",
            FatKind::Fat32 => "fat32",
        };
        assert_eq!(volume.format(), name);
        let geometry = volume.geometry().unwrap();
        assert_eq!(geometry.sector_size, 512);
        assert_eq!(geometry.allocation_unit, volume.boot().cluster_size());
        assert_eq!(geometry.label.as_deref(), Some("LAYERS"));
        assert_eq!(geometry.serial.as_deref(), Some("1234-ABCD"));
        let probe = Volume::<Owned>::probe(&Owned(built.image.clone())).unwrap();
        assert_eq!(probe.format, name);
        assert_eq!(probe.declared_bytes, Some(size));
        let copies = volume.superblocks().unwrap();
        assert_eq!(copies.len(), if kind == FatKind::Fat32 { 2 } else { 1 });
        assert!(copies.iter().all(|c| c.integrity == Integrity::Verified));
    }
}

#[test]
fn names_are_matched_without_regard_to_case() {
    let built = build(FatKind::Fat16, 8 * MIB);
    let volume = Volume::open(Owned(built.image)).unwrap();
    let exact = volume.lookup("/Long File Name Here.txt").unwrap();
    assert_eq!(volume.lookup("/long file name here.TXT").unwrap(), exact);
    assert_eq!(
        volume
            .lookup("/DOCS/Nested/DEEP.BIN")
            .unwrap()
            .first_cluster(),
        {
            volume
                .lookup("/docs/nested/deep.bin")
                .unwrap()
                .first_cluster()
        }
    );
    assert!(volume.lookup("/docs/missing").is_err());
}

#[test]
fn deleted_entries_are_candidates_with_their_run_of_clusters() {
    for (kind, size) in SIZES {
        let built = build(kind, size);
        let volume = Volume::open(Owned(built.image.clone())).unwrap();
        let candidates = volume.salvage().unwrap().all_candidates().unwrap();
        let gone = candidates
            .iter()
            .find(|c| c.state == CandidateState::Deleted && c.detail.size == 3000)
            .unwrap_or_else(|| panic!("{kind:?}: no deleted 3000-byte file in {candidates:#?}"));
        assert_eq!(gone.kind, CandidateKind::File);
        let walk = volume.salvage().unwrap().walk_extents(gone).unwrap();
        assert_eq!(walk.claimed, 3000);
        let mapped: u64 = walk.sources().map(|extent| extent.length).sum();
        assert_eq!(mapped, 3000, "{kind:?}: {walk:?}");
        let source = volume.source();
        let mut recovered = Vec::new();
        let mut buffer = vec![0u8; 3000];
        for extent in walk.sources() {
            let read = volume
                .salvage()
                .unwrap()
                .read_extent(extent, 0, &mut buffer[..extent.length as usize])
                .unwrap();
            recovered.extend_from_slice(&buffer[..read]);
        }
        let _ = source;
        assert_eq!(recovered, pattern(3000, 7), "{kind:?}");
    }
}

#[test]
fn a_missing_long_name_checksum_fails_the_candidate_and_keeps_the_short_name() {
    let built = build(FatKind::Fat16, 8 * MIB);
    let mut image = built.image;
    // The long name entry of "Long File Name Here.txt" sits just before its
    // short entry in the fixed root; break the checksum byte it carries.
    let volume = Volume::open(Owned(image.clone())).unwrap();
    let candidates = volume.salvage().unwrap().all_candidates().unwrap();
    let target = candidates
        .iter()
        .find(|c| c.name.as_deref() == Some("Long File Name Here.txt"))
        .unwrap();
    assert_eq!(target.integrity, Integrity::Verified);
    assert_eq!(target.detail.long_name_ok, Some(true));
    let short_at = target.location.offset as usize;
    image[short_at - 32 + 13] ^= 0xFF;
    let volume = Volume::open_for_salvage(Owned(image)).unwrap();
    let candidates = volume.salvage().unwrap().all_candidates().unwrap();
    let broken = candidates
        .iter()
        .find(|c| c.location == target.location)
        .unwrap();
    assert_eq!(broken.integrity, Integrity::Failed);
    assert_eq!(broken.detail.long_name_ok, Some(false));
    assert_eq!(
        broken.name.as_deref(),
        Some(broken.detail.short_name.as_str())
    );
}

#[test]
fn a_directory_that_lost_its_parent_is_found_by_its_dot_entries() {
    let built = build(FatKind::Fat16, 8 * MIB);
    let mut image = built.image;
    let volume = Volume::open(Owned(image.clone())).unwrap();
    let candidates = volume.salvage().unwrap().all_candidates().unwrap();
    let nested = candidates
        .iter()
        .find(|c| c.name.as_deref() == Some("nested"))
        .unwrap();
    // Erase the entry that names "docs/nested" so no tree reaches it.
    let at = nested.location.offset as usize;
    image[at..at + 32].fill(0);
    let volume = Volume::open_for_salvage(Owned(image)).unwrap();
    let salvage = volume.salvage().unwrap();
    let after = salvage.all_candidates().unwrap();
    let deep = after
        .iter()
        .find(|c| c.name.as_deref() == Some("deep.bin"))
        .expect("deep.bin is found through the orphan directory's own cluster");
    let tree = salvage.rebuild_tree(&after).unwrap();
    let node = tree
        .nodes
        .iter()
        .find(|node| node.candidate == deep.location.offset)
        .unwrap();
    assert!(node.parents.is_empty(), "no entry names its directory now");
    assert!(tree
        .orphans()
        .any(|node| node.candidate == deep.location.offset));
}

#[test]
fn salvage_opens_through_the_backup_boot_sector_and_says_so() {
    let built = build(FatKind::Fat32, 40 * MIB);
    let mut image = built.image;
    image[..512].fill(0);
    assert!(Volume::open(Owned(image.clone())).is_err());
    let volume = Volume::open_for_salvage(Owned(image.clone())).unwrap();
    assert_eq!(volume.boot_sector_offset(), 6 * 512);
    let copies = volume.superblocks().unwrap();
    assert_eq!(copies[0].integrity, Integrity::Failed);
    assert!(copies[0].superblock.is_none());
    assert_eq!(copies[1].integrity, Integrity::Verified);
    assert_eq!(volume.health().unwrap().state, HealthState::Damaged);
    let probe = Volume::<Owned>::probe(&Owned(image)).unwrap();
    assert_eq!(probe.recognition, fs_core::layers::Recognition::Damaged);
}

#[test]
fn a_fat12_or_fat16_volume_without_a_boot_sector_has_no_geometry() {
    let built = build(FatKind::Fat12, 2 * MIB);
    let mut image = built.image;
    image[..512].fill(0);
    assert!(Volume::open_for_salvage(Owned(image.clone())).is_err());
    assert!(matches!(
        FatAdapter::from_source(&Owned(image)),
        Err(fs_core::layers::SalvageError::NoGeometry)
    ));
}

#[test]
fn the_adapter_reports_what_it_cannot_do() {
    let built = build(FatKind::Fat12, 2 * MIB);
    let volume = Volume::open(Owned(built.image)).unwrap();
    let adapter = volume.adapter().unwrap();
    let missing = adapter.capabilities().missing;
    assert!(missing.contains(&fs_core::layers::Feature::EncodedExtents));
    assert!(missing.contains(&fs_core::layers::Feature::GeometryHypotheses));
    assert_eq!(missing.len(), 2);
}

// ------------------------------------------------- orphaned directory clusters

fn entry(name: &[u8; 11], first: u16, size: u32) -> [u8; 32] {
    let mut raw = [0u8; 32];
    raw[..11].copy_from_slice(name);
    raw[11] = 0x20;
    raw[26..28].copy_from_slice(&first.to_le_bytes());
    raw[28..32].copy_from_slice(&size.to_le_bytes());
    raw
}

/// The two highest free clusters of a healthy image.
fn free_clusters(image: &[u8]) -> (u32, u32) {
    let volume = Volume::open(Owned(image.to_vec())).unwrap();
    let mut free = (2..volume.boot().cluster_count + 2).rev().filter(|c| {
        matches!(
            volume.fat_value(*c),
            Ok(fs_fat32::primitives::FatValue::Free)
        )
    });
    (free.next().unwrap(), free.next().unwrap())
}

fn put(image: &mut [u8], cluster: u32, bytes: &[u8]) {
    let at = Volume::open(Owned(image.to_vec()))
        .unwrap()
        .cluster_offset(cluster)
        .unwrap() as usize;
    image[at..at + bytes.len()].copy_from_slice(bytes);
}

fn candidates_of(image: Vec<u8>) -> Vec<fs_core::layers::Candidate<fs_fat32::layers::FatDetail>> {
    Volume::open_for_salvage(Owned(image))
        .unwrap()
        .salvage()
        .unwrap()
        .all_candidates()
        .unwrap()
}

#[test]
fn entries_in_an_orphaned_cluster_with_no_dot_entries_are_found_with_lower_confidence() {
    for (kind, size) in SIZES {
        let built = build(kind, size);
        let (directory, data) = free_clusters(&built.image);
        let mut run = Vec::new();
        run.extend_from_slice(&entry(b"ORPHAN  TXT", data as u16, 5));
        run.extend_from_slice(&entry(b"SECOND  BIN", 0, 0));
        for allocated in [true, false] {
            let mut image = built.image.clone();
            put(&mut image, directory, &run);
            put(&mut image, data, b"hello");
            if allocated {
                set_entry(&mut image, kind, directory, 0x0FFF_FFFF);
            }
            let found = candidates_of(image);
            let orphan = found
                .iter()
                .find(|c| c.name.as_deref() == Some("ORPHAN.TXT"))
                .unwrap_or_else(|| panic!("{kind:?}: ORPHAN.TXT is not found"));
            assert_eq!(orphan.detail.found_by, fs_fat32::layers::FoundBy::Content);
            assert_eq!(orphan.detail.directory, directory);
            assert_eq!(orphan.integrity, Integrity::Unchecked);
            assert_eq!(
                orphan.state,
                if allocated {
                    CandidateState::Live
                } else {
                    CandidateState::Deleted
                }
            );
            // Reachable entries keep their stronger evidence.
            let hello = found
                .iter()
                .find(|c| c.name.as_deref() == Some("hello.txt"))
                .unwrap();
            assert_eq!(hello.integrity, Integrity::Verified);
            assert_eq!(hello.detail.found_by, fs_fat32::layers::FoundBy::Tree);
        }
    }
}

#[test]
fn file_data_that_resembles_an_entry_is_not_reported_as_a_directory() {
    for (kind, size) in SIZES {
        let built = build(kind, size);
        let (cluster, _) = free_clusters(&built.image);
        let lone = entry(b"LOOKS   TXT", 0, 0);
        let cases: [(&str, Vec<u8>); 3] = [
            ("one entry beside a stray long-name entry", {
                let mut stray = [0u8; 32];
                stray[0] = 0x41;
                stray[11] = 0x0F;
                stray[13] = 0x99;
                [stray.to_vec(), lone.to_vec()].concat()
            }),
            (
                "one entry among text",
                [
                    lone.to_vec(),
                    b"The quick brown fox jumps over the lazy dog. ".repeat(8),
                ]
                .concat(),
            ),
            (
                "entries among bytes that are not",
                [
                    lone.to_vec(),
                    entry(b"ALSO    TXT", 0, 0).to_vec(),
                    vec![0xFF; 32],
                ]
                .concat(),
            ),
        ];
        for (name, bytes) in cases {
            let mut image = built.image.clone();
            put(&mut image, cluster, &bytes);
            set_entry(&mut image, kind, cluster, 0x0FFF_FFFF);
            let found = candidates_of(image);
            assert!(
                found
                    .iter()
                    .all(|c| c.detail.found_by == fs_fat32::layers::FoundBy::Tree
                        || c.detail.directory != cluster),
                "{kind:?} {name}: reported as a directory"
            );
        }
    }
}

#[test]
fn a_scan_that_finds_orphaned_clusters_is_the_same_cut_at_any_range() {
    let built = build(FatKind::Fat16, 8 * MIB);
    let (directory, _) = free_clusters(&built.image);
    let mut image = built.image.clone();
    let mut run = entry(b"ORPHAN  TXT", 0, 0).to_vec();
    run.extend_from_slice(&entry(b"SECOND  BIN", 0, 0));
    put(&mut image, directory, &run);
    set_entry(&mut image, FatKind::Fat16, directory, 0xFFFF);
    let volume = Volume::open_for_salvage(Owned(image.clone())).unwrap();
    let salvage = volume.salvage().unwrap();
    let whole = salvage.all_candidates().unwrap();
    assert!(whole
        .iter()
        .any(|c| c.name.as_deref() == Some("ORPHAN.TXT")));
    for piece in [512u64, 4096, 5000, 65_536] {
        let mut joined = Vec::new();
        let mut at = 0;
        while at < image.len() as u64 {
            let length = piece.min(image.len() as u64 - at);
            salvage
                .find_candidates(Extent::new(at, length), &mut joined)
                .unwrap();
            at += length;
        }
        assert!(
            joined == whole,
            "pieces of {piece} bytes differ from the whole scan"
        );
    }
}

#[test]
fn a_lone_entry_in_an_orphaned_cluster_is_found_as_the_weakest_candidate() {
    for (kind, size) in SIZES {
        let built = build(kind, size);
        let (directory, data) = free_clusters(&built.image);
        let mut image = built.image.clone();
        put(
            &mut image,
            directory,
            &entry(b"ORPHAN  TXT", data as u16, 5),
        );
        put(&mut image, data, b"hello");
        set_entry(&mut image, kind, directory, 0x0FFF_FFFF);
        let found = candidates_of(image.clone());
        let orphan = found
            .iter()
            .find(|c| c.name.as_deref() == Some("ORPHAN.TXT"))
            .unwrap_or_else(|| panic!("{kind:?}: the lone ORPHAN.TXT is not found"));
        assert_eq!(orphan.detail.found_by, fs_fat32::layers::FoundBy::Content);
        assert!(orphan.detail.lone_entry, "{kind:?}");
        assert_eq!(orphan.detail.directory, directory);
        assert_eq!(orphan.integrity, Integrity::Unchecked);
        // Two entries are stronger evidence: no lone flag.
        put(
            &mut image,
            directory,
            &[
                entry(b"ORPHAN  TXT", data as u16, 5),
                entry(b"SECOND  BIN", 0, 0),
            ]
            .concat(),
        );
        let two = candidates_of(image);
        assert!(two
            .iter()
            .filter(|c| c.detail.directory == directory)
            .all(|c| !c.detail.lone_entry));
        // Reachable entries are never marked lone.
        assert!(found
            .iter()
            .filter(|c| c.detail.directory != directory)
            .all(|c| !c.detail.lone_entry));
    }
}

// ---------------------------------------------------- deleted-file cluster guess

use fs_fat32::layers::DeletedGuess;

/// What the deleted entry `name` rebuilds as under `guess`, and how its walk
/// ended.
fn rebuilt(
    image: &[u8],
    name: &str,
    guess: DeletedGuess,
) -> (
    Vec<u8>,
    fs_core::layers::WalkEnd,
    fs_core::layers::ExtentWalk,
) {
    let source = Owned(image.to_vec());
    let adapter = FatAdapter::from_source(&source)
        .unwrap()
        .with_deleted_guess(guess);
    let mut found = Vec::new();
    adapter
        .find_candidates(&source, Extent::new(0, source.size_bytes()), &mut found)
        .unwrap();
    let candidate = found
        .iter()
        .find(|c| {
            c.state == CandidateState::Deleted
                && c.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .unwrap_or_else(|| panic!("no deleted {name}"));
    let walk = adapter.walk_extents(&source, candidate).unwrap();
    let mut bytes = Vec::new();
    adapter
        .materialize(&source, &walk.plan(), &mut bytes)
        .unwrap();
    (bytes, walk.end, walk)
}

/// A volume with `/D.BIN` of three clusters deleted, written around a live
/// `/LIVE.BIN` that sits between its first cluster and the rest.
fn written_around(kind: FatKind, size: u64) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let disk = Disk::new(vec![0; size as usize]);
    format(
        &disk,
        &FormatOptions {
            kind: Some(kind),
            ..FormatOptions::default()
        },
    )
    .unwrap();
    let cluster = u64::from(
        fs_fat32::BootSector::parse(&disk.bytes()[..512])
            .unwrap()
            .cluster_size() as u32,
    ) as usize;
    let wanted = pattern(3 * cluster - 7, 9);
    let live = pattern(cluster, 10);
    let mut volume = WriteVolume::mount(disk.clone()).unwrap();
    let d = volume.create_file("/D.BIN").unwrap();
    volume.write(&d, 0, &wanted[..cluster]).unwrap();
    let l = volume.create_file("/LIVE.BIN").unwrap();
    volume.write(&l, 0, &live).unwrap();
    volume
        .write(&d, cluster as u64, &wanted[cluster..])
        .unwrap();
    volume.unlink("/D.BIN").unwrap();
    volume.unmount().unwrap();
    (disk.bytes(), wanted, live)
}

#[test]
fn a_deleted_file_written_around_a_live_one_is_recovered_by_the_free_cluster_guess() {
    for (kind, size) in SIZES {
        let (image, wanted, live) = written_around(kind, size);
        let (bytes, end, walk) = rebuilt(&image, "_.BIN", DeletedGuess::FreeClusters);
        assert_eq!(end, fs_core::layers::WalkEnd::Complete, "{kind:?}");
        assert!(bytes == wanted, "{kind:?}: not byte for byte");
        assert!(
            walk.extents.len() >= 2,
            "{kind:?}: the live cluster is skipped"
        );
        // The plain run walks into the live file's cluster.
        let (plain, end, _) = rebuilt(&image, "_.BIN", DeletedGuess::Contiguous);
        assert_eq!(end, fs_core::layers::WalkEnd::Broken, "{kind:?}");
        assert!(plain != wanted, "{kind:?}");
        assert!(
            !plain.windows(live.len()).any(|w| w == live)
                || end == fs_core::layers::WalkEnd::Broken
        );
    }
}

#[test]
fn a_contiguous_deleted_file_gives_the_same_walk_under_both_guesses() {
    for (kind, size) in SIZES {
        let built = build(kind, size);
        let free = rebuilt(&built.image, "_ONE.TXT", DeletedGuess::FreeClusters);
        let plain = rebuilt(&built.image, "_ONE.TXT", DeletedGuess::Contiguous);
        assert_eq!(free.2, plain.2, "{kind:?}");
        assert_eq!(free.0, plain.0, "{kind:?}");
        assert_eq!(free.1, fs_core::layers::WalkEnd::Complete);
        assert_eq!(free.0, pattern(3000, 7), "{kind:?}");
    }
}

#[test]
fn a_deleted_file_whose_gap_was_reused_by_another_deleted_file_is_a_known_limit() {
    for (kind, size) in SIZES {
        let disk = Disk::new(vec![0; size as usize]);
        format(
            &disk,
            &FormatOptions {
                kind: Some(kind),
                ..FormatOptions::default()
            },
        )
        .unwrap();
        let mut volume = WriteVolume::mount(disk.clone()).unwrap();
        let first = pattern(1500, 1);
        let second = pattern(1500, 2);
        let d = volume.create_file("/D.BIN").unwrap();
        volume.write(&d, 0, &first).unwrap();
        volume.unlink("/D.BIN").unwrap();
        volume.unmount().unwrap();
        // Another file, since deleted too, used D's cluster: what it left
        // there is all that is there.
        let mut image = disk.bytes();
        let volume = Volume::open(Owned(image.clone())).unwrap();
        let first_cluster = volume
            .salvage()
            .unwrap()
            .all_candidates()
            .unwrap()
            .iter()
            .find(|c| c.state == CandidateState::Deleted && c.detail.size == 1500)
            .unwrap()
            .detail
            .first_cluster;
        put(&mut image, first_cluster, &second);
        // Both clusters are free again, and D's entry cannot tell whose
        // bytes they hold: it reads F's. The guess documents this limit.
        let (bytes, _, _) = rebuilt(&image, "_.BIN", DeletedGuess::FreeClusters);
        assert_eq!(bytes, second, "{kind:?}");
        assert_ne!(bytes, first, "{kind:?}");
    }
}

#[test]
fn the_free_cluster_guess_is_partial_when_the_volume_ends() {
    let (mut image, _, _) = written_around(FatKind::Fat16, 8 * MIB);
    // Claim a size that no free run to the end of the volume can cover.
    let volume = Volume::open(Owned(image.clone())).unwrap();
    let candidates = volume.salvage().unwrap().all_candidates().unwrap();
    let gone = candidates
        .iter()
        .find(|c| c.state == CandidateState::Deleted && c.detail.size > 0)
        .unwrap();
    let at = gone.location.offset as usize + 28;
    image[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let (_, end, walk) = rebuilt(&image, "_.BIN", DeletedGuess::FreeClusters);
    assert_eq!(end, fs_core::layers::WalkEnd::OutOfRange);
    assert!(walk.mapped() < walk.claimed);
}
