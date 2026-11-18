//! The scenario every oracle test runs, and how the driver's own view of a
//! volume is taken, shared by the Linux and Windows oracle tests.
#![allow(dead_code)]

use super::sha256::sha256_hex;
use fs_core::OwnedRwSlice;
use fs_fat32::mkfs::FormatOptions;
use fs_fat32::timestamp::FatTimestamp;
use fs_fat32::write::{open_writable_path, EntryInfo, WriteVolume};
use fs_fat32::FatKind;
use std::collections::BTreeMap;
use std::path::Path;

pub const MIB: u64 = 1024 * 1024;

pub fn pattern(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

pub type Volume = WriteVolume<OwnedRwSlice>;

pub fn clock() -> FatTimestamp {
    // 2024-03-05 14:07:09.34 UTC: an odd second, so the creation tenths matter.
    FatTimestamp::from_unix(1_709_647_629, 340)
}

pub fn later_clock() -> FatTimestamp {
    // 2025-06-30 23:59:58 UTC
    FatTimestamp::from_unix(1_751_327_998, 0)
}

/// Mounts the volume at `offset` for writing with the fixed clock.
pub fn mount(path: &Path, offset: u64) -> Volume {
    let mut volume = open_writable_path(path, offset).unwrap();
    volume.set_clock(clock);
    volume
}

/// What the driver says is on the volume: path to (info, sha256 of contents).
pub type View = BTreeMap<String, (EntryInfo, String)>;

pub fn view(path: &Path, offset: u64) -> View {
    let mut volume = open_writable_path(path, offset).unwrap();
    let mut out = View::new();
    fn descend(volume: &mut Volume, directory: &str, out: &mut View) {
        for info in volume.read_dir(directory).unwrap() {
            let path = if directory.is_empty() {
                info.name.clone()
            } else {
                format!("{directory}/{}", info.name)
            };
            let sha = if info.is_dir {
                descend(volume, &path, out);
                "-".to_string()
            } else {
                let handle = volume.open(&path).unwrap();
                let mut data = vec![0u8; info.size as usize];
                let mut done = 0;
                while done < data.len() {
                    done += volume
                        .read(&handle, done as u64, &mut data[done..])
                        .unwrap();
                }
                sha256_hex(&data)
            };
            out.insert(path, (info, sha));
        }
    }
    descend(&mut volume, "", &mut out);
    out
}

/// Geometry with several clusters per file, and a different cluster size for
/// each variant.
pub fn options(kind: FatKind) -> (FormatOptions, u64) {
    let (spc, size) = match kind {
        FatKind::Fat12 => (2, 1440 * 1024),
        FatKind::Fat16 => (4, 16 * MIB),
        FatKind::Fat32 => (1, 34 * MIB),
    };
    (
        FormatOptions {
            kind: Some(kind),
            sectors_per_cluster: Some(spc),
            serial: Some(0xFA7_0001),
            label: Some("WRITTEN".into()),
            size: Some(size),
            ..FormatOptions::default()
        },
        size,
    )
}

pub fn kind_name(kind: FatKind) -> &'static str {
    match kind {
        FatKind::Fat12 => "fat12",
        FatKind::Fat16 => "fat16",
        FatKind::Fat32 => "fat32",
    }
}

pub struct Step {
    pub name: &'static str,
    pub run: Box<dyn Fn(&mut Volume)>,
}

pub fn step(name: &'static str, run: impl Fn(&mut Volume) + 'static) -> Step {
    Step {
        name,
        run: Box::new(run),
    }
}

/// The scenario: every operation of the write API, in an order where each
/// builds on the last. FAT12, FAT16 and FAT32 all run all of it.
pub fn scenario() -> Vec<Step> {
    vec![
        step("create-empty-file", |v| {
            v.create_file("empty.txt").unwrap();
        }),
        step("write-small-file", |v| {
            let f = v.create_file("small.txt").unwrap();
            v.write(&f, 0, &pattern(100, 1)).unwrap();
        }),
        step("write-multi-cluster-file", |v| {
            let f = v.create_file("multi.bin").unwrap();
            v.write(&f, 0, &pattern(70_000, 2)).unwrap();
        }),
        step("mkdir-nested", |v| {
            v.mkdir("docs").unwrap();
            v.mkdir("docs/nested").unwrap();
            let f = v.create_file("docs/nested/deep.txt").unwrap();
            v.write(&f, 0, &pattern(3000, 3)).unwrap();
        }),
        step("nt-case-names", |v| {
            for name in ["lower.txt", "UPPER.TXT", "mixed.TXT", "NOEXT", "x"] {
                v.create_file(name).unwrap();
            }
        }),
        step("long-names-with-a-short-name-collision", |v| {
            for name in [
                "A long file name with spaces.txt",
                "A long file name with spaces too.txt",
                "A long file name with spaces three.txt",
                "Mixed Case.Txt",
                "Readme.md",
            ] {
                let f = v.create_file(name).unwrap();
                v.write(&f, 0, name.as_bytes()).unwrap();
            }
        }),
        step("long-names-without-a-collision", |v| {
            for name in [
                "Ünïcödé résumé.txt",
                "日本語のファイル.txt",
                ".hidden-file",
                "a+b,c;d=e.dat",
            ] {
                let f = v.create_file(name).unwrap();
                v.write(&f, 0, name.as_bytes()).unwrap();
            }
        }),
        step("overwrite-in-place", |v| {
            let f = v.open("multi.bin").unwrap();
            v.set_clock(later_clock);
            v.write(&f, 1000, &pattern(5000, 9)).unwrap();
        }),
        step("append-and-sparse-write", |v| {
            let f = v.open("small.txt").unwrap();
            v.write(&f, 100, &pattern(2000, 4)).unwrap();
            v.write(&f, 9000, &pattern(50, 5)).unwrap();
        }),
        step("truncate-shorter", |v| {
            let f = v.open("multi.bin").unwrap();
            v.truncate(&f, 20_000).unwrap();
        }),
        step("truncate-longer", |v| {
            let f = v.open("multi.bin").unwrap();
            v.truncate(&f, 30_000).unwrap();
        }),
        step("truncate-to-zero", |v| {
            let f = v.open("docs/nested/deep.txt").unwrap();
            v.truncate(&f, 0).unwrap();
        }),
        step("rename-in-a-directory", |v| {
            v.rename("Readme.md", "Readme renamed to something long.md")
                .unwrap();
            v.rename("lower.txt", "LOWER2.TXT").unwrap();
        }),
        step("rename-across-directories", |v| {
            v.rename("small.txt", "docs/moved small file.txt").unwrap();
        }),
        step("rename-a-directory", |v| {
            v.mkdir("other").unwrap();
            v.rename("docs/nested", "other/nested again").unwrap();
        }),
        step("unlink", |v| {
            v.unlink("empty.txt").unwrap();
            v.unlink("UPPER.TXT").unwrap();
        }),
        step("rmdir", |v| {
            v.unlink("other/nested again/deep.txt").unwrap();
            v.rmdir("other/nested again").unwrap();
        }),
        step("fragmented-file", |v| {
            for index in 0..24 {
                let f = v.create_file(&format!("frag{index:02}.tmp")).unwrap();
                v.write(&f, 0, &pattern(3000, index as u8)).unwrap();
            }
            for index in (0..24).step_by(2) {
                v.unlink(&format!("frag{index:02}.tmp")).unwrap();
            }
            let f = v.create_file("fragmented.bin").unwrap();
            v.write(&f, 0, &pattern(40_000, 77)).unwrap();
        }),
        step("directory-growth", |v| {
            v.mkdir("crowded").unwrap();
            for index in 0..70 {
                let f = v
                    .create_file(&format!(
                        "crowded/a rather long file name number {index:03}.dat"
                    ))
                    .unwrap();
                v.write(&f, 0, &[index as u8; 40]).unwrap();
            }
        }),
        step("root-directory-growth", |v| {
            for index in 0..30 {
                v.create_file(&format!("root entry with a long name {index:03}.txt"))
                    .ok();
            }
        }),
    ]
}
