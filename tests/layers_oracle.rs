//! The three layers against volumes that are not this driver's own work.
//!
//! `validation/generate.sh` builds FAT12, FAT16 and FAT32 images with
//! `mkfs.fat`, fills them through the Linux kernel's vfat driver, and records
//! what the kernel, mtools and `fsck.fat` say about them (see
//! `tests/independent_validation.rs`). Here rust-fs-core's conformance checks
//! run over the same images, and the layers' verdicts are held against those
//! records: the files the kernel lists are the files `FileAPI` lists, with
//! the bytes mtools copied out; a volume `fsck.fat` calls clean reports no
//! damage; one it refuses reports the damage it names; the entries the kernel
//! removed are deleted candidates whose clusters hold the removed bytes.
//!
//! Needs the oracles, so it is behind the `oracle` feature and FAILS (never
//! skips) when they cannot run: `chore test:oracle`.
#![cfg(feature = "oracle")]

mod common;

use common::{fixtures, fsck_exit, image, records};
use fs_core::layers::conformance::{
    check_damaged, check_files_against_candidates, check_healthy, Expected, Limits,
};
use fs_core::layers::{
    CandidateState, DamageKind, FileAPI, HealthState, NodeKind, OpenVolume, PrimitiveAPI, Severity,
    VolumeAPI, WalkEnd,
};
use fs_core::FileDevice;
use fs_fat32::Volume;

const KINDS: [&str; 3] = ["fat12", "fat16", "fat32"];

fn open(name: &str) -> Volume<FileDevice> {
    Volume::open(FileDevice::open(image(name)).unwrap()).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// What the kernel's mount and mtools hold: every directory, and every file
/// with the bytes mtools copied out.
fn expected(name: &str, tree: &str) -> Vec<Expected> {
    let manifest = records(name, "manifest");
    let mut out = Vec::new();
    for fields in manifest.get("d").into_iter().flatten() {
        out.push(Expected::directory(&format!("/{}", fields[1])));
    }
    for fields in manifest.get("f").into_iter().flatten() {
        let bytes = std::fs::read(fixtures().join(tree).join(&fields[1])).unwrap();
        assert_eq!(bytes.len() as u64, fields[0].parse::<u64>().unwrap());
        out.push(Expected::file(&format!("/{}", fields[1]), &bytes));
    }
    out
}

fn findings(volume: &Volume<FileDevice>) -> Vec<fs_core::layers::Damage> {
    let mut findings = Vec::new();
    volume
        .salvage()
        .unwrap()
        .report_damage(&mut findings)
        .unwrap();
    findings
}

#[test]
fn clean_images_conform_and_report_no_damage() {
    for name in KINDS {
        assert_eq!(fsck_exit(name), 0, "{name}: fsck.fat calls the image clean");
        let volume = open(name);
        let limits = Limits::default();
        let mut report = check_healthy(&volume, &expected(name, &format!("{name}.tree")), &limits);
        report.merge(check_files_against_candidates(&volume, &limits));
        assert!(report.passed(), "{name}:\n{report}");
        assert_eq!(volume.health().unwrap().state, HealthState::Clean, "{name}");
        let found = findings(&volume);
        assert!(found.is_empty(), "{name}: {found:#?}");
    }
}

#[test]
fn removed_entries_are_deleted_candidates_holding_the_removed_bytes() {
    for base in KINDS {
        let name = format!("{base}-deleted");
        assert_eq!(fsck_exit(&name), 0, "{name}: removal through the kernel");
        let volume = open(&name);
        let limits = Limits::default();
        let mut report = check_healthy(&volume, &expected(&name, &format!("{name}.tree")), &limits);
        report.merge(check_files_against_candidates(&volume, &limits));
        assert!(report.passed(), "{name}:\n{report}");

        let salvage = volume.salvage().unwrap();
        let candidates = salvage.all_candidates().unwrap();
        let originals = records(base, "manifest");
        let chains: std::collections::BTreeMap<&str, Vec<u32>> = originals
            .get("chain")
            .into_iter()
            .flatten()
            .map(|fields| {
                let clusters = fields[1]
                    .split(',')
                    .filter_map(|n| n.parse().ok())
                    .collect();
                (fields[0].as_str(), clusters)
            })
            .collect();
        let recipe = records(&name, "recipe");
        let mut matched = 0;
        for fields in &recipe["deleted"] {
            let (size, path) = (fields[0].parse::<u64>().unwrap(), fields[1].as_str());
            let same_size: Vec<_> = candidates
                .iter()
                .filter(|c| c.state == CandidateState::Deleted && c.size == Some(size))
                .collect();
            assert!(
                !same_size.is_empty(),
                "{name}: no deleted candidate for {path} ({size})"
            );
            let contiguous = chains
                .get(path)
                .is_none_or(|clusters| clusters.windows(2).all(|pair| pair[1] == pair[0] + 1));
            let original =
                std::fs::read(fixtures().join(format!("{base}.tree")).join(path)).unwrap();
            let mut rebuilt = false;
            for deleted in same_size {
                let walk = salvage.walk_extents(deleted).unwrap();
                assert_eq!(walk.claimed, size, "{name}: {path}");
                if walk.end == WalkEnd::Complete {
                    let mut bytes = Vec::new();
                    salvage.materialize(&walk.plan(), &mut bytes).unwrap();
                    rebuilt |= bytes == original;
                }
            }
            if contiguous {
                assert!(rebuilt, "{name}: {path} rebuilds as it was");
                matched += 1;
            }
        }
        assert!(matched > 0, "{name}: at least one deleted file is rebuilt");
    }
}

#[test]
fn a_truncated_chain_is_reported_the_way_fsck_names_it() {
    for base in KINDS {
        let name = format!("{base}-truncated");
        assert_ne!(fsck_exit(&name), 0, "{name}: fsck.fat sees the damage");
        let volume = open(&name);
        let report = check_damaged(&volume, &Limits::default());
        assert!(report.passed(), "{name}:\n{report}");
        let found = findings(&volume);
        let broken: Vec<_> = found
            .iter()
            .filter(|d| d.kind == DamageKind::Broken && d.severity == Severity::Error)
            .collect();
        assert!(
            broken.iter().any(|d| d.detail.contains("deep.bin")),
            "{name}: {found:#?}"
        );
        // Only that file: everything else reads as it does in the clean image.
        assert_eq!(broken.len(), 1, "{name}: {found:#?}");

        let salvage = volume.salvage().unwrap();
        let candidates = salvage.all_candidates().unwrap();
        let deep = candidates
            .iter()
            .find(|c| c.name.as_deref() == Some("deep.bin"))
            .unwrap();
        let walk = salvage.walk_extents(deep).unwrap();
        assert_eq!(walk.end, WalkEnd::Broken, "{name}");
        assert!(walk.mapped() < walk.claimed, "{name}: {walk:?}");

        let file = volume.lookup("/docs/nested/deep.bin").unwrap();
        let error = volume
            .copy_to(&file, &mut std::io::sink())
            .expect_err("a chain shorter than the size must not read");
        assert!(error.to_string().contains("chain ends before file size"));
    }
}

#[test]
fn a_missing_signature_is_a_warning_the_kernel_and_fsck_accept() {
    for base in KINDS {
        let name = format!("{base}-nobootsig");
        let volume = open(&name);
        let report = check_damaged(&volume, &Limits::default());
        assert!(report.passed(), "{name}:\n{report}");
        let health = volume.health().unwrap();
        assert_eq!(health.state, HealthState::Degraded, "{name}: {health:?}");
        assert!(
            findings(&volume)
                .iter()
                .any(|d| d.kind == DamageKind::Signature && d.severity == Severity::Warning),
            "{name}"
        );
        // The files are all there: the namespace is whole.
        let root = volume.root().unwrap();
        assert!(!volume.read_dir(&root).unwrap().is_empty(), "{name}");
        assert!(volume
            .read_dir(&root)
            .unwrap()
            .iter()
            .any(|entry| entry.kind == NodeKind::Directory));
    }
}

#[test]
fn a_zeroed_boot_sector_opens_for_salvage_only_where_a_backup_exists() {
    for base in KINDS {
        let name = format!("{base}-zeroboot");
        assert!(
            Volume::open(FileDevice::open(image(&name)).unwrap()).is_err(),
            "{name}: the strict open refuses it"
        );
        let salvage = Volume::open_for_salvage(FileDevice::open(image(&name)).unwrap());
        if base != "fat32" {
            assert!(salvage.is_err(), "{name}: FAT12 and FAT16 keep no backup");
            continue;
        }
        let volume = salvage.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(volume.boot_sector_offset(), 6 * 512, "{name}");
        let report = check_damaged(&volume, &Limits::default());
        assert!(report.passed(), "{name}:\n{report}");
        assert_eq!(volume.health().unwrap().state, HealthState::Damaged);
        // The namespace is intact behind the dead primary boot sector, and
        // the backup's geometry reads it as it reads the clean image.
        let tree = format!("{base}.tree");
        let report = check_files_against_candidates(&volume, &Limits::default());
        assert!(report.passed(), "{name}:\n{report}");
        for want in expected(base, &tree) {
            if let Expected::File { path, contents } = want {
                let node = volume.lookup(&path).unwrap();
                let mut bytes = Vec::new();
                volume.copy_to(&node, &mut bytes).unwrap();
                assert!(bytes == contents, "{name}: {path}");
            }
        }
    }
}
