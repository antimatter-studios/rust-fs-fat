//! The formatter against things that are not the formatter.
//!
//! Every image is made by `fs_fat32::mkfs` into the oracle work directory and
//! then checked by `fsck.fat -n` (dosfstools) and mounted read-only by the
//! Linux kernel's vfat driver, which must agree with the layout the formatter
//! planned. Sizes cover each variant and the cluster counts on either side of
//! each variant boundary. Needs the oracles, so it is behind the `oracle`
//! feature and FAILS (never skips) when they cannot run.
#![cfg(feature = "oracle")]

mod oracle;

use fs_fat32::mkfs::{format_path, plan, FormatOptions, Layout};
use fs_fat32::FatKind;
use std::path::PathBuf;

const MIB: u64 = 1024 * 1024;
const WORK: &str = "fat-format";

struct Case {
    name: String,
    options: FormatOptions,
    bytes: u64,
}

fn case(name: &str, bytes: u64, options: FormatOptions) -> Case {
    Case {
        name: name.to_string(),
        options: FormatOptions {
            serial: Some(0x1234_ABCD),
            ..options
        },
        bytes,
    }
}

fn kind(kind: FatKind) -> FormatOptions {
    FormatOptions {
        kind: Some(kind),
        ..FormatOptions::default()
    }
}

/// The sizes, in bytes, whose cluster count with one sector per cluster is at
/// the edge of `boundary`: the largest and smallest forming sizes in a sweep.
fn edge_sizes(variant: FatKind, boundary: u64) -> (u64, u64) {
    let options = FormatOptions {
        sectors_per_cluster: Some(1),
        ..kind(variant)
    };
    let forming: Vec<u64> = (boundary.saturating_sub(5)..boundary + 1_300)
        .filter(|total| plan(total * 512, &options).is_ok())
        .collect();
    (forming[0] * 512, *forming.last().unwrap() * 512)
}

fn cases() -> Vec<Case> {
    let mut cases = vec![
        case(
            "fat12-floppy",
            1440 * 1024,
            FormatOptions {
                media: 0xF0,
                ..kind(FatKind::Fat12)
            },
        ),
        case("fat12-auto-2m", 2 * MIB, FormatOptions::default()),
        case("fat12-auto-15m", 15 * MIB, FormatOptions::default()),
        case("fat16-auto-16m", 16 * MIB, FormatOptions::default()),
        case("fat16-auto-100m", 100 * MIB, FormatOptions::default()),
        case("fat16-forced-30m", 30 * MIB, kind(FatKind::Fat16)),
        case(
            "fat16-one-fat",
            20 * MIB,
            FormatOptions {
                fat_count: 1,
                ..kind(FatKind::Fat16)
            },
        ),
        case(
            "fat16-labelled",
            20 * MIB,
            FormatOptions {
                label: Some("Recovery".into()),
                ..kind(FatKind::Fat16)
            },
        ),
        case(
            "fat16-bps1024",
            20 * MIB,
            FormatOptions {
                bytes_per_sector: 1024,
                ..kind(FatKind::Fat16)
            },
        ),
        case("fat32-forced-40m", 40 * MIB, kind(FatKind::Fat32)),
        case("fat32-auto-600m", 600 * MIB, FormatOptions::default()),
        case("fat32-auto-3g", 3072 * MIB, FormatOptions::default()),
        case(
            "fat32-labelled",
            64 * MIB,
            FormatOptions {
                label: Some("DATA 1".into()),
                ..kind(FatKind::Fat32)
            },
        ),
        case(
            "fat32-bps4096",
            600 * MIB,
            FormatOptions {
                bytes_per_sector: 4096,
                ..kind(FatKind::Fat32)
            },
        ),
        case(
            "fat32-spc8",
            300 * MIB,
            FormatOptions {
                sectors_per_cluster: Some(8),
                ..kind(FatKind::Fat32)
            },
        ),
    ];
    // One sector per cluster: the total size sets the cluster count, so the
    // sizes either side of each threshold sit exactly on it.
    let one = |variant| FormatOptions {
        sectors_per_cluster: Some(1),
        ..kind(variant)
    };
    for (label, variant, boundary, upper) in [
        ("fat12-top", FatKind::Fat12, 4_084, true),
        ("fat16-bottom", FatKind::Fat16, 4_085, false),
        ("fat16-top", FatKind::Fat16, 65_524, true),
        ("fat32-bottom", FatKind::Fat32, 65_525, false),
    ] {
        let (smallest, largest) = edge_sizes(variant, boundary);
        let bytes = if upper { largest } else { smallest };
        cases.push(case(label, bytes, one(variant)));
        // The neighbouring size, one sector more or less, on the same variant
        // when it still forms.
        let neighbour = if upper { bytes - 512 } else { bytes + 512 };
        cases.push(case(&format!("{label}-next"), neighbour, one(variant)));
    }
    cases
}

fn clusters_of(layout: &Layout) -> u64 {
    u64::from(layout.cluster_count)
}

#[test]
fn fsck_and_the_kernel_accept_every_formatted_image() {
    let dir = oracle::work_dir(WORK);
    let cases = cases();
    let mut images = Vec::new();
    let mut layouts = Vec::new();
    for case in &cases {
        let path: PathBuf = dir.join(format!("{}.img", case.name));
        let _ = std::fs::remove_file(&path);
        let options = FormatOptions {
            size: Some(case.bytes),
            ..case.options.clone()
        };
        let layout = format_path(&path, &options)
            .unwrap_or_else(|e| panic!("{}: format failed: {e}", case.name));
        images.push(path);
        layouts.push(layout);
    }
    let (checks, tools) = oracle::check_images(WORK, &images);
    eprintln!("tools:\n{tools}");
    for ((case, layout), check) in cases.iter().zip(&layouts).zip(&checks) {
        let name = &case.name;
        assert_eq!(
            check.fsck_exit, 0,
            "{name}: fsck.fat -n is not clean ({layout:?}):\n{}",
            check.fsck_output
        );
        assert!(
            check.mounted,
            "{name}: the kernel refused to mount it: {}",
            check.mount_error
        );
        let (block, total, free) = check.statfs.expect("statfs");
        assert_eq!(block, layout.cluster_bytes(), "{name}: cluster size");
        assert_eq!(total, clusters_of(layout), "{name}: cluster count");
        let used = u64::from(layout.kind == FatKind::Fat32);
        assert_eq!(free, total - used, "{name}: free clusters");
        assert_eq!(check.fs_type, "msdos", "{name}: {}", check.fs_type);
        let label = layout.has_label.then(|| {
            String::from_utf8_lossy(&layout.label)
                .trim_end()
                .to_string()
        });
        assert_eq!(check.entries, Some(0), "{name}: the root holds nothing");
        if let Some(label) = label {
            assert!(
                check.label.starts_with(&label),
                "{name}: label {:?}",
                check.label
            );
        }
    }
    // The boundary cases landed in the variant and cluster counts intended.
    let find = |name: &str| {
        let at = cases.iter().position(|c| c.name == name).unwrap();
        &layouts[at]
    };
    assert_eq!(find("fat12-top").cluster_count, 4084);
    assert_eq!(find("fat16-bottom").cluster_count, 4085);
    assert_eq!(find("fat16-top").cluster_count, 65_524);
    assert_eq!(find("fat32-bottom").cluster_count, 65_525);
}
