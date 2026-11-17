//! The driver against things that are not the driver.
//!
//! `validation/generate.sh` builds FAT12, FAT16 and FAT32 images with
//! `mkfs.fat`, fills them through the Linux kernel's vfat driver, and checks
//! the kernel's view against an mtools (`mcopy`) copy of the same image. This
//! file compares what `fs_fat32` reports with that: names, sizes, bytes and
//! cluster chains. It then repeats the exercise on damaged copies.
//!
//! Needs the oracles, so it is behind the `oracle` feature and FAILS (never
//! skips) when they cannot run. `chore test:oracle` boots the Linux test VM.
#![cfg(feature = "oracle")]

use fs_core::{FileDevice, ReadOnlyDevice};
use fs_fat32::directory::{parse_directory, scan_directory, DirectoryEntry};
use fs_fat32::file::FileAPI;
use fs_fat32::primitives::{FatValue, PrimitiveAPI};
use fs_fat32::volume::{open_path, open_raw_device, Volume, VolumeAPI};
use fs_fat32::FatKind;
mod common;

use common::{assert_unwritten, fixtures, fsck_exit, image, records, snapshot, text};
use std::collections::BTreeMap;

const KINDS: [(&str, FatKind); 3] = [
    ("fat12", FatKind::Fat12),
    ("fat16", FatKind::Fat16),
    ("fat32", FatKind::Fat32),
];

// ------------------------------------------------------- reading with ours

type Vol = Volume<fs_core::OwnedSlice>;

#[derive(Debug)]
struct Node {
    directory: bool,
    size: u64,
    entry: DirectoryEntry,
}

fn directory_bytes(volume: &Vol, first_cluster: Option<u32>) -> Vec<u8> {
    let boot = volume.boot();
    match first_cluster {
        None if boot.kind != FatKind::Fat32 => {
            let start = (u64::from(boot.reserved_sectors)
                + u64::from(boot.fat_count) * u64::from(boot.sectors_per_fat))
                * u64::from(boot.bytes_per_sector);
            let mut bytes = vec![
                0;
                (u64::from(boot.root_dir_sectors) * u64::from(boot.bytes_per_sector))
                    as usize
            ];
            volume.read_volume_bytes(start, &mut bytes).unwrap();
            bytes
        }
        first => {
            let first = first.unwrap_or(boot.root_cluster);
            let mut bytes = Vec::new();
            for cluster in volume
                .chain_candidates(first, boot.cluster_count as usize)
                .unwrap()
            {
                let mut data = vec![0; boot.cluster_size() as usize];
                volume.read_cluster(cluster, &mut data).unwrap();
                bytes.extend(data);
            }
            bytes
        }
    }
}

/// Every live path in the volume, found with the driver's own parsers.
fn walk(volume: &Vol) -> BTreeMap<String, Node> {
    fn descend(
        volume: &Vol,
        prefix: &str,
        entries: Vec<DirectoryEntry>,
        out: &mut BTreeMap<String, Node>,
    ) {
        for entry in entries {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let path = format!("{prefix}{}", entry.name);
            let directory = entry.attributes & 0x10 != 0;
            out.insert(
                path.clone(),
                Node {
                    directory,
                    size: if directory { 0 } else { u64::from(entry.size) },
                    entry: entry.clone(),
                },
            );
            if directory {
                let bytes = directory_bytes(volume, Some(entry.first_cluster));
                let children = parse_directory(&bytes, volume.boot().kind)
                    .unwrap()
                    .into_iter()
                    .filter(|e| !e.deleted)
                    .collect();
                descend(volume, &format!("{path}/"), children, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    descend(volume, "", volume.root_directory().unwrap(), &mut out);
    out
}

fn read_all(volume: &Vol, entry: &DirectoryEntry) -> fs_fat32::Result<Vec<u8>> {
    let file = volume.open_file(entry)?;
    let mut data = vec![0; file.len() as usize];
    let mut done = 0;
    while done < data.len() {
        let n = file.read_at(done as u64, &mut data[done..])?;
        assert!(n > 0, "read_at made no progress");
        done += n;
    }
    Ok(data)
}

fn open(name: &str) -> Vol {
    open_path(&image(name), 0).unwrap_or_else(|e| panic!("open {name}: {e}"))
}

fn clusters_of(volume: &Vol, node: &Node) -> Vec<u32> {
    let needed = node.size.div_ceil(volume.boot().cluster_size()) as usize;
    if needed == 0 {
        return Vec::new();
    }
    volume
        .chain_candidates(node.entry.first_cluster, needed)
        .unwrap()
}

/// What the independent readers say the volume holds.
fn expected(name: &str) -> BTreeMap<String, (bool, u64)> {
    let manifest = records(name, "manifest");
    let mut out = BTreeMap::new();
    for kind in ["d", "f"] {
        for fields in manifest.get(kind).into_iter().flatten() {
            out.insert(fields[1].clone(), (kind == "d", fields[0].parse().unwrap()));
        }
    }
    out
}

fn listing(nodes: &BTreeMap<String, Node>) -> BTreeMap<String, (bool, u64)> {
    nodes
        .iter()
        .map(|(p, n)| (p.clone(), (n.directory, n.size)))
        .collect()
}

// ------------------------------------------------------------------- tests

#[test]
fn clean_images_list_and_read_like_the_kernel_and_mtools() {
    for (name, kind) in KINDS {
        let before = snapshot(name);
        assert_eq!(
            fsck_exit(name),
            0,
            "{name}: fsck.fat must call the image clean"
        );
        let volume = open(name);
        assert_eq!(volume.boot().kind, kind, "{name}: variant");

        let nodes = walk(&volume);
        assert_eq!(
            listing(&nodes),
            expected(name),
            "{name}: driver listing differs from the kernel/mtools view"
        );

        let tree = fixtures().join(format!("{name}.tree"));
        for (path, node) in nodes.iter().filter(|(_, n)| !n.directory) {
            let want = std::fs::read(tree.join(path)).unwrap();
            let got =
                read_all(&volume, &node.entry).unwrap_or_else(|e| panic!("{name}: {path}: {e}"));
            assert!(
                want == got,
                "{name}: {path}: content differs from the oracle copy"
            );
        }
        assert_unwritten(name, &before);
    }
}

#[test]
fn cluster_chains_match_mtools_mshowfat_including_fragmented_files() {
    for (name, _) in KINDS {
        let volume = open(name);
        let nodes = walk(&volume);
        let chains = records(name, "manifest");
        let mut fragmented = 0;
        for fields in &chains["chain"] {
            let path = &fields[0];
            let theirs: Vec<u32> = fields[1]
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.parse().unwrap())
                .collect();
            let ours = clusters_of(&volume, &nodes[path]);
            assert_eq!(ours, theirs, "{name}: {path}: chain differs from mshowfat");
            if theirs.windows(2).any(|w| w[1] != w[0] + 1) {
                fragmented += 1;
            }
        }
        assert!(
            fragmented >= 2,
            "{name}: fixture must hold fragmented files"
        );
        for path in ["frag-a.bin", "frag-b.bin"] {
            let chain = clusters_of(&volume, &nodes[path]);
            assert!(
                chain.windows(2).all(|w| w[1] != w[0] + 1),
                "{name}: {path} interleaved"
            );
        }
    }
}

#[test]
fn root_layouts_are_the_ones_each_variant_defines() {
    // FAT12 packed entries: a chain visits both odd and even clusters, which
    // is where the 12-bit packing differs.
    let volume = open("fat12");
    let nodes = walk(&volume);
    let chain = clusters_of(&volume, &nodes["docs/nested/deep.bin"]);
    assert!(chain.iter().any(|c| c % 2 == 0) && chain.iter().any(|c| c % 2 == 1));

    // FAT16: a fixed root outside the data area, no root cluster.
    let volume = open("fat16");
    assert_eq!(volume.boot().kind, FatKind::Fat16);
    assert_eq!(volume.boot().root_cluster, 0);
    assert!(volume.boot().root_entry_count > 0);

    // FAT32: the root is a cluster chain, and ours is more than one cluster.
    let volume = open("fat32");
    let boot = volume.boot();
    assert_eq!(boot.root_entry_count, 0);
    let root_chain = volume
        .chain_candidates(boot.root_cluster, boot.cluster_count as usize)
        .unwrap();
    assert!(
        root_chain.len() > 1,
        "FAT32 root must be chained: {root_chain:?}"
    );
    let live = volume.root_directory().unwrap();
    assert!(live.len() > (boot.cluster_size() / 32) as usize / 3);
    assert!(matches!(
        volume.fat_value(*root_chain.last().unwrap()).unwrap(),
        FatValue::EndOfChain
    ));
}

#[test]
fn deleted_records_are_hidden_from_the_listing_and_kept_by_the_forensic_scan() {
    for (base, _) in KINDS {
        let name = format!("{base}-deleted");
        let before = snapshot(&name);
        assert_eq!(
            fsck_exit(&name),
            0,
            "{name}: removal through the kernel is clean"
        );
        let volume = open(&name);
        let nodes = walk(&volume);
        assert_eq!(listing(&nodes), expected(&name), "{name}: live listing");

        let recipe = records(&name, "recipe");
        let mut root_sizes = Vec::new();
        let mut nested_sizes = Vec::new();
        for fields in &recipe["deleted"] {
            let size: u32 = fields[0].parse().unwrap();
            assert!(
                !nodes.contains_key(&fields[1]),
                "{name}: {} must be gone",
                fields[1]
            );
            if fields[1].contains('/') {
                nested_sizes.push(size);
            } else {
                root_sizes.push(size);
            }
        }
        let scan = |bytes: Vec<u8>| -> Vec<u32> {
            let mut sizes: Vec<u32> = scan_directory(&bytes, volume.boot().kind)
                .unwrap()
                .into_iter()
                .filter(|r| r.entry.deleted && r.raw[0] == 0xE5)
                .map(|r| r.entry.size)
                .collect();
            sizes.sort_unstable();
            sizes
        };
        root_sizes.sort_unstable();
        nested_sizes.sort_unstable();
        assert_eq!(
            scan(directory_bytes(&volume, None)),
            root_sizes,
            "{name}: root"
        );
        let nested = &nodes["docs/nested"].entry;
        let nested_scan = scan(directory_bytes(&volume, Some(nested.first_cluster)));
        for size in &nested_sizes {
            assert!(
                nested_scan.contains(size),
                "{name}: nested deleted record {size}"
            );
        }
        assert_unwritten(&name, &before);
    }
}

#[test]
fn a_truncated_chain_is_refused_for_that_file_only() {
    for (base, _) in KINDS {
        let name = format!("{base}-truncated");
        let before = snapshot(&name);
        assert_ne!(fsck_exit(&name), 0, "{name}: fsck.fat must see the damage");
        assert!(
            text(&name, "fsck").to_lowercase().contains("deep.bin"),
            "{name}: fsck names the file"
        );

        let volume = open(&name);
        let nodes = walk(&volume);
        let tree = fixtures().join(format!("{base}.tree"));
        for (path, node) in nodes.iter().filter(|(_, n)| !n.directory) {
            let result = read_all(&volume, &node.entry);
            if path == "docs/nested/deep.bin" {
                let error = result.expect_err("a chain shorter than the size must not read");
                assert!(
                    error.to_string().contains("chain ends before file size"),
                    "{error}"
                );
            } else {
                let want = std::fs::read(tree.join(path)).unwrap();
                assert!(result.unwrap() == want, "{name}: {path}");
            }
        }
        assert_unwritten(&name, &before);
    }
}

#[test]
fn damaged_boot_metadata_is_refused_by_volume_but_readable_raw() {
    for (base, _) in KINDS {
        for (recipe, oracle_rejects) in [("nobootsig", false), ("zeroboot", true), ("badbpb", true)]
        {
            let name = format!("{base}-{recipe}");
            let before = snapshot(&name);
            // The oracle: fsck.fat accepts a missing signature and refuses
            // a zero sector size or an empty boot sector.
            assert_eq!(
                fsck_exit(&name) != 0,
                oracle_rejects,
                "{name}: fsck.fat verdict"
            );

            // A refused open is an error path too: it must not write.
            assert!(
                open_path(&image(&name), 0).is_err(),
                "{name}: Volume::open must refuse"
            );
            assert_unwritten(&name, &before);

            // The raw layer needs no boot sector: every byte reads back.
            let raw = open_raw_device(
                ReadOnlyDevice::new(FileDevice::open(image(&name)).unwrap()),
                0,
            )
            .unwrap();
            assert_eq!(raw.size_bytes(), before.len() as u64);
            let mut offset = 0usize;
            let mut chunk = vec![0u8; 1 << 20];
            while offset < before.len() {
                let n = chunk.len().min(before.len() - offset);
                raw.read_volume_bytes(offset as u64, &mut chunk[..n])
                    .unwrap();
                assert!(
                    chunk[..n] == before[offset..offset + n],
                    "{name}: raw bytes at {offset}"
                );
                offset += n;
            }
            assert!(raw
                .read_volume_bytes(before.len() as u64 - 1, &mut [0; 2])
                .is_err());
            assert_unwritten(&name, &before);
        }
    }
}
