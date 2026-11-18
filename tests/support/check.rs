//! An independent structural check of a FAT image, written without the
//! driver: it parses the BPB, the FATs and the directories itself and panics
//! with a reason when anything is inconsistent. It returns the file tree it
//! found, with contents, so tests compare that with what they expect.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};

pub struct Node {
    pub is_dir: bool,
    pub size: u64,
    pub data: Vec<u8>,
    pub short: String,
    pub first: u32,
}

pub struct Report {
    pub tree: BTreeMap<String, Node>,
    pub free_clusters: u32,
    pub cluster_count: u32,
    pub kind: &'static str,
}

struct Geometry<'a> {
    image: &'a [u8],
    bps: usize,
    cluster_bytes: usize,
    clusters: u32,
    kind: &'static str,
    fat: &'a [u8],
    data_start: usize,
    root: u32,
}

impl Geometry<'_> {
    fn entry(&self, cluster: u32) -> u32 {
        let c = cluster as usize;
        match self.kind {
            "fat12" => {
                let at = c + c / 2;
                let word = u16::from_le_bytes([self.fat[at], self.fat[at + 1]]);
                u32::from(if c.is_multiple_of(2) {
                    word & 0xFFF
                } else {
                    word >> 4
                })
            }
            "fat16" => u32::from(u16::from_le_bytes([self.fat[c * 2], self.fat[c * 2 + 1]])),
            _ => u32::from_le_bytes(self.fat[c * 4..c * 4 + 4].try_into().unwrap()) & 0x0FFF_FFFF,
        }
    }

    fn is_end(&self, value: u32) -> bool {
        match self.kind {
            "fat12" => value >= 0xFF8,
            "fat16" => value >= 0xFFF8,
            _ => value >= 0x0FFF_FFF8,
        }
    }

    fn chain(&self, first: u32, what: &str) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut current = first;
        while current != 0 {
            assert!(
                current >= 2 && current < self.clusters + 2,
                "{what}: chain leaves the data area at {current}"
            );
            assert!(chain.len() <= self.clusters as usize, "{what}: chain loops");
            chain.push(current);
            let next = self.entry(current);
            if self.is_end(next) {
                return chain;
            }
            assert!(
                next >= 2,
                "{what}: chain runs into a free or reserved entry ({next})"
            );
            current = next;
        }
        chain
    }

    fn cluster(&self, cluster: u32) -> &[u8] {
        let at = self.data_start + (cluster as usize - 2) * self.cluster_bytes;
        &self.image[at..at + self.cluster_bytes]
    }

    fn read_chain(&self, chain: &[u32]) -> Vec<u8> {
        chain
            .iter()
            .flat_map(|c| self.cluster(*c).iter().copied())
            .collect()
    }
}

fn checksum(short: &[u8]) -> u8 {
    let mut sum = 0u8;
    for byte in short {
        sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(*byte);
    }
    sum
}

fn short_text(raw: &[u8]) -> String {
    let base = String::from_utf8_lossy(&raw[..8]).trim_end().to_string();
    let extension = String::from_utf8_lossy(&raw[8..11]).trim_end().to_string();
    if extension.is_empty() {
        base
    } else {
        format!("{base}.{extension}")
    }
}

pub fn check(image: &[u8], expect_clean: bool) -> Report {
    let word = |at: usize| u16::from_le_bytes([image[at], image[at + 1]]) as usize;
    let dword = |at: usize| u32::from_le_bytes(image[at..at + 4].try_into().unwrap()) as usize;
    assert_eq!(&image[510..512], &[0x55, 0xAA], "boot signature");
    let bps = word(11);
    let spc = image[13] as usize;
    let reserved = word(14);
    let fats = image[16] as usize;
    let root_entries = word(17);
    let total = if word(19) != 0 { word(19) } else { dword(32) };
    let spf = if word(22) != 0 { word(22) } else { dword(36) };
    let root_sectors = (root_entries * 32).div_ceil(bps);
    let data_sector = reserved + fats * spf + root_sectors;
    let clusters = ((total - data_sector) / spc) as u32;
    let kind = if clusters <= 4084 {
        "fat12"
    } else if clusters <= 65_524 {
        "fat16"
    } else {
        "fat32"
    };
    let fat_bytes = spf * bps;
    let copy = |n: usize| &image[(reserved + n * spf) * bps..(reserved + (n + 1) * spf) * bps];
    let mirroring = kind != "fat32" || image[40] & 0x80 == 0;
    if mirroring {
        for n in 1..fats {
            assert!(copy(0) == copy(n), "FAT copy {n} differs from copy 0");
        }
    }
    let root_id = if kind == "fat32" { dword(44) as u32 } else { 0 };
    let active = if mirroring {
        0
    } else {
        (image[40] & 0xF) as usize
    };
    let geometry = Geometry {
        image,
        bps,
        cluster_bytes: bps * spc,
        clusters,
        kind,
        fat: &copy(active)[..fat_bytes],
        data_start: data_sector * bps,
        root: root_id,
    };
    assert!(
        (match kind {
            "fat12" => (clusters as usize + 2) * 3 / 2 + 1,
            "fat16" => (clusters as usize + 2) * 2,
            _ => (clusters as usize + 2) * 4,
        }) <= fat_bytes,
        "the FAT is too small"
    );
    // The FAT's first two entries and the clean-shutdown bits.
    if kind == "fat16" {
        let one = geometry.entry(1);
        assert_eq!(one & 0x8000 != 0, expect_clean, "FAT16 clean bit");
        assert!(one & 0x4000 != 0, "FAT16 error bit");
    }
    if kind == "fat32" {
        let one = geometry.entry(1);
        assert_eq!(one & 0x0800_0000 != 0, expect_clean, "FAT32 clean bit");
        assert!(one & 0x0400_0000 != 0, "FAT32 error bit");
    }
    if kind != "fat12" {
        let state = image[if kind == "fat32" { 65 } else { 37 }];
        assert_eq!(state & 1 == 0, expect_clean, "boot sector dirty bit");
    }

    let mut owners: HashMap<u32, String> = HashMap::new();
    let mut tree = BTreeMap::new();
    let root_bytes = if kind == "fat32" {
        let chain = geometry.chain(root_id, "root");
        for c in &chain {
            assert!(owners.insert(*c, "/".into()).is_none(), "root cross-linked");
        }
        geometry.read_chain(&chain)
    } else {
        image[(reserved + fats * spf) * bps..data_sector * bps].to_vec()
    };
    walk(
        &geometry,
        &root_bytes,
        "",
        root_id,
        0,
        &mut owners,
        &mut tree,
        kind == "fat32",
    );

    // Every allocated cluster belongs to exactly one thing.
    let mut used = 0u32;
    for cluster in 2..clusters + 2 {
        let value = geometry.entry(cluster);
        if value != 0 {
            used += 1;
            assert!(
                owners.contains_key(&cluster),
                "cluster {cluster} is allocated but nothing owns it"
            );
        }
    }
    assert_eq!(
        used as usize,
        owners.len(),
        "allocated clusters against owned clusters"
    );
    let free = clusters - used;
    if kind == "fat32" {
        let info = word(48) * bps;
        let at = |o: usize| u32::from_le_bytes(image[info + o..info + o + 4].try_into().unwrap());
        assert_eq!(at(0), 0x4161_5252, "FSInfo lead signature");
        assert_eq!(at(484), 0x6141_7272, "FSInfo structure signature");
        assert_eq!(at(488), free, "FSInfo free count against the FAT");
        let hint = at(492);
        assert!(
            hint == 0xFFFF_FFFF || (2..clusters + 2).contains(&hint),
            "FSInfo hint {hint}"
        );
        assert_eq!(at(508), 0xAA55_0000, "FSInfo trail signature");
    }
    Report {
        tree,
        free_clusters: free,
        cluster_count: clusters,
        kind,
    }
}

#[allow(clippy::too_many_arguments)]
fn walk(
    g: &Geometry,
    bytes: &[u8],
    prefix: &str,
    id: u32,
    parent: u32,
    owners: &mut HashMap<u32, String>,
    tree: &mut BTreeMap<String, Node>,
    chained_root: bool,
) {
    let _ = chained_root;
    let mut seen_end = false;
    let mut pending: Option<(u8, u8, Vec<u16>)> = None; // next ordinal, checksum, units (reversed order)
    let mut names: Vec<String> = Vec::new();
    let mut shorts: Vec<[u8; 11]> = Vec::new();
    let mut index = 0usize;
    let mut first_real = true;
    for (slot, raw) in bytes.chunks_exact(32).enumerate() {
        if raw[0] == 0 {
            seen_end = true;
            assert!(
                raw.iter().all(|b| *b == 0),
                "{prefix}: end marker entry is not zero"
            );
            continue;
        }
        assert!(
            !seen_end,
            "{prefix}: live data after the end marker at slot {slot}"
        );
        if raw[0] == 0xE5 {
            pending = None;
            continue;
        }
        if raw[11] == 0x0F {
            let ordinal = raw[0] & 0x1F;
            assert!((1..=20).contains(&ordinal), "{prefix}: bad ordinal");
            match &mut pending {
                None => {
                    assert!(
                        raw[0] & 0x40 != 0,
                        "{prefix}: first long entry is not flagged last"
                    );
                    pending = Some((ordinal, raw[13], Vec::new()));
                }
                Some((next, check, _)) => {
                    assert_eq!(raw[0] & 0x40, 0, "{prefix}: a second 'last' long entry");
                    assert_eq!(ordinal, *next - 1, "{prefix}: long entries out of order");
                    assert_eq!(
                        raw[13], *check,
                        "{prefix}: long entries disagree on the checksum"
                    );
                    *next = ordinal;
                }
            }
            let (_, _, units) = pending.as_mut().unwrap();
            let mut fragment = Vec::new();
            for p in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30] {
                fragment.push(u16::from_le_bytes([raw[p], raw[p + 1]]));
            }
            assert_eq!(&raw[26..28], &[0, 0], "{prefix}: long entry cluster field");
            // Fragments arrive last first; keep them in file order.
            fragment.extend(units.iter().copied());
            *units = fragment;
            continue;
        }
        // A short entry.
        let name_bytes = &raw[..11];
        assert!(
            name_bytes
                .iter()
                .all(|b| *b >= 0x20 && !b"\"*+,/:;<=>?[\\]|".contains(b)),
            "{prefix}: short name has an illegal byte {name_bytes:?}"
        );
        let mut display = short_text(raw);
        if raw[12] & 0x08 != 0 || raw[12] & 0x10 != 0 {
            let (base, ext) = match display.split_once('.') {
                Some((b, e)) => (b.to_string(), e.to_string()),
                None => (display.clone(), String::new()),
            };
            let base = if raw[12] & 0x08 != 0 {
                base.to_lowercase()
            } else {
                base
            };
            let ext = if raw[12] & 0x10 != 0 {
                ext.to_lowercase()
            } else {
                ext
            };
            display = if ext.is_empty() {
                base
            } else {
                format!("{base}.{ext}")
            };
        }
        let mut name = display.clone();
        if let Some((ordinal, check, units)) = pending.take() {
            assert_eq!(ordinal, 1, "{prefix}: long entries stop before ordinal 1");
            assert_eq!(
                check,
                checksum(name_bytes),
                "{prefix}: long name checksum for {display}"
            );
            let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
            assert!(
                units[end..].iter().skip(1).all(|u| *u == 0xFFFF),
                "{prefix}: long name padding"
            );
            name = String::from_utf16(&units[..end]).expect("long name is UTF-16");
        }
        if raw[11] & 0x08 != 0 && raw[11] != 0x0F {
            continue; // the volume label
        }
        let first = (if g.kind == "fat32" {
            u32::from(u16::from_le_bytes([raw[20], raw[21]])) << 16
        } else {
            assert_eq!(
                &raw[20..22],
                &[0, 0],
                "{prefix}: high cluster word on FAT12/16"
            );
            0
        }) | u32::from(u16::from_le_bytes([raw[26], raw[27]]));
        let size = u32::from_le_bytes(raw[28..32].try_into().unwrap());
        let is_dir = raw[11] & 0x10 != 0;
        if id != 0 || g.kind == "fat32" {
            // Subdirectory (or FAT32 root): `.` and `..` come first in
            // subdirectories only.
            if name == "." || name == ".." {
                assert!(is_dir && slot < 2, "{prefix}: misplaced {name}");
                let want = if name == "." { id } else { parent };
                assert_eq!(
                    first, want,
                    "{prefix}: {name} points at {first}, wanted {want}"
                );
                index += 1;
                continue;
            }
        }
        let _ = (&mut first_real, index);
        let key = format!("{prefix}/{name}");
        let lower = name.to_lowercase();
        assert!(
            !names.contains(&lower),
            "{key}: duplicate name in a directory"
        );
        names.push(lower);
        assert!(
            !shorts.contains(&raw[..11].try_into().unwrap()),
            "{key}: duplicate short name"
        );
        shorts.push(raw[..11].try_into().unwrap());
        if is_dir {
            assert_eq!(size, 0, "{key}: directory size");
            assert!(first >= 2, "{key}: directory without a cluster");
            let chain = g.chain(first, &key);
            for c in &chain {
                assert!(
                    owners.insert(*c, key.clone()).is_none(),
                    "{key}: cross-linked cluster {c}"
                );
            }
            let data = g.read_chain(&chain);
            tree.insert(
                key.clone(),
                Node {
                    is_dir: true,
                    size: 0,
                    data: Vec::new(),
                    short: display,
                    first,
                },
            );
            walk(
                g,
                &data,
                &key,
                first,
                if id == g.root { 0 } else { id },
                owners,
                tree,
                false,
            );
        } else {
            let chain = g.chain(first, &key);
            assert_eq!(
                chain.len(),
                (size as usize).div_ceil(g.cluster_bytes),
                "{key}: chain length against size {size}"
            );
            if size == 0 {
                assert_eq!(first, 0, "{key}: empty file with a cluster");
            }
            for c in &chain {
                assert!(
                    owners.insert(*c, key.clone()).is_none(),
                    "{key}: cross-linked cluster {c}"
                );
            }
            let mut data = g.read_chain(&chain);
            data.truncate(size as usize);
            tree.insert(
                key,
                Node {
                    is_dir: false,
                    size: u64::from(size),
                    data,
                    short: display,
                    first,
                },
            );
        }
    }
    assert!(
        pending.is_none(),
        "{prefix}: long entries with no short entry"
    );
    let _ = g.bps;
}
