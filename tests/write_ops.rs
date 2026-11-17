//! The file operations on FAT12, FAT16 and FAT32 volumes, each result checked
//! by an independent structural check of the bytes (`support::check`), which
//! parses the volume without the driver.

mod support;

use fs_fat32::mkfs::{format, FormatOptions};
use fs_fat32::timestamp::FatTimestamp;
use fs_fat32::write::{open_writable, FileHandle, WriteVolume};
use fs_fat32::{Error, FatKind};
use std::collections::BTreeMap;
use support::check::{check, Report};
use support::{pattern, Disk};

const MIB: usize = 1024 * 1024;
const KINDS: [FatKind; 3] = [FatKind::Fat12, FatKind::Fat16, FatKind::Fat32];

/// A volume of one sector per cluster, so small files make long chains.
fn fresh(kind: FatKind) -> Disk {
    let bytes = match kind {
        FatKind::Fat12 => 1440 * 1024,
        FatKind::Fat16 => 8 * MIB,
        FatKind::Fat32 => 34 * MIB,
    };
    let disk = Disk::new(vec![0u8; bytes]);
    format(
        &disk,
        &FormatOptions {
            kind: Some(kind),
            sectors_per_cluster: Some(1),
            serial: Some(0x5EED),
            ..FormatOptions::default()
        },
    )
    .unwrap();
    disk
}

fn mount(disk: &Disk) -> WriteVolume<Disk> {
    let mut volume = WriteVolume::mount(disk.clone()).unwrap();
    volume.set_clock(clock);
    volume
}

fn clock() -> FatTimestamp {
    // 2024-03-05 14:07:09 UTC
    FatTimestamp::from_unix(1_709_647_629, 0)
}

fn later() -> FatTimestamp {
    // 2025-06-30 23:59:58 UTC
    FatTimestamp::from_unix(1_751_327_998, 0)
}

/// The volume as an independent reader sees it, after a clean close.
fn closed(volume: WriteVolume<Disk>, disk: &Disk) -> Report {
    volume.unmount().unwrap();
    check(&disk.bytes(), true)
}

fn contents(report: &Report) -> BTreeMap<String, Option<Vec<u8>>> {
    report
        .tree
        .iter()
        .map(|(path, node)| (path.clone(), (!node.is_dir).then(|| node.data.clone())))
        .collect()
}

fn model(entries: &[(&str, Option<Vec<u8>>)]) -> BTreeMap<String, Option<Vec<u8>>> {
    entries
        .iter()
        .map(|(path, data)| (path.to_string(), data.clone()))
        .collect()
}

fn read_all(volume: &WriteVolume<Disk>, handle: &FileHandle) -> Vec<u8> {
    let mut data = vec![0u8; volume.len(handle).unwrap() as usize];
    let mut done = 0;
    while done < data.len() {
        let n = volume.read(handle, done as u64, &mut data[done..]).unwrap();
        assert!(n > 0);
        done += n;
    }
    data
}

#[test]
fn a_fresh_volume_passes_the_independent_check() {
    for kind in KINDS {
        let disk = fresh(kind);
        let report = check(&disk.bytes(), true);
        assert!(report.tree.is_empty());
        let volume = mount(&disk);
        let expected = report.cluster_count - u32::from(kind == FatKind::Fat32);
        assert_eq!(volume.free_clusters(), expected, "{kind:?}");
    }
}

#[test]
fn files_of_every_size_round_trip() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        let sizes = [0usize, 1, 511, 512, 513, 1024, 5000, 40_000];
        let mut expected = Vec::new();
        for (index, size) in sizes.iter().enumerate() {
            let name = format!("file{index}.bin");
            let data = pattern(*size, index as u8);
            let handle = volume.create_file(&name).unwrap();
            assert_eq!(volume.write(&handle, 0, &data).unwrap(), data.len());
            assert_eq!(read_all(&volume, &handle), data);
            expected.push((name, Some(data)));
        }
        let before = volume.free_clusters();
        assert!(before < 1_000_000);
        let report = closed(volume, &disk);
        let want: Vec<(&str, Option<Vec<u8>>)> = expected
            .iter()
            .map(|(n, d)| (n.as_str(), d.clone()))
            .collect();
        let want: Vec<(String, Option<Vec<u8>>)> = want
            .iter()
            .map(|(n, d)| (format!("/{n}"), d.clone()))
            .collect();
        let want: BTreeMap<_, _> = want.into_iter().collect();
        assert_eq!(contents(&report), want, "{kind:?}");
        assert_eq!(report.free_clusters, before);
    }
}

#[test]
fn directories_nest_and_dot_entries_point_where_they_should() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        volume.mkdir("docs").unwrap();
        volume.mkdir("docs/deep").unwrap();
        assert!(volume.mkdir("/docs/missing/deeper").is_err());
        volume.mkdir("docs/deep/deeper").unwrap();
        let file = volume.create_file("docs/deep/deeper/leaf.txt").unwrap();
        volume.write(&file, 0, b"leaf").unwrap();
        let top = volume.create_file("docs/top.txt").unwrap();
        volume.write(&top, 0, b"top").unwrap();
        assert_eq!(volume.read_dir("/").unwrap().len(), 1);
        let names: Vec<String> = volume
            .read_dir("docs")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["deep", "top.txt"]);
        let report = closed(volume, &disk);
        assert_eq!(
            contents(&report),
            model(&[
                ("/docs", None),
                ("/docs/deep", None),
                ("/docs/deep/deeper", None),
                ("/docs/deep/deeper/leaf.txt", Some(b"leaf".to_vec())),
                ("/docs/top.txt", Some(b"top".to_vec())),
            ]),
            "{kind:?}"
        );
    }
}

#[test]
fn mkdir_and_create_refuse_what_already_exists_in_any_case() {
    let disk = fresh(FatKind::Fat16);
    let mut volume = mount(&disk);
    volume.create_file("Report.TXT").unwrap();
    volume.mkdir("Folder").unwrap();
    for name in ["Report.TXT", "report.txt", "REPORT.TXT", "FOLDER", "folder"] {
        assert!(
            matches!(volume.create_file(name), Err(Error::Exists)),
            "{name}"
        );
        assert!(matches!(volume.mkdir(name), Err(Error::Exists)), "{name}");
    }
    assert!(matches!(
        volume.create_file("Folder/x/y"),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        volume.create_file("Report.TXT/x"),
        Err(Error::NotADirectory)
    ));
    assert!(matches!(volume.create_file(""), Err(Error::InvalidName(_))));
    assert!(matches!(
        volume.create_file("/"),
        Err(Error::InvalidName(_))
    ));
    assert!(matches!(
        volume.create_file("a/../b"),
        Err(Error::InvalidName(_))
    ));
    closed(volume, &disk);
}

#[test]
fn eight_dot_three_names_carry_case_in_the_nt_flags_and_need_no_long_entries() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        for name in [
            "lower.txt",
            "UPPER.TXT",
            "mixed.TXT",
            "UPEXT.txt",
            "noext",
            "A",
        ] {
            volume.create_file(name).unwrap();
        }
        let report = closed(volume, &disk);
        let image = disk.bytes();
        // No long-name entries: nothing in the root has the 0x0F attribute.
        let root = root_bytes(&image, kind);
        assert!(root.chunks_exact(32).all(|e| e[11] != 0x0F), "{kind:?}");
        let mut names: Vec<&String> = report.tree.keys().collect();
        names.sort();
        assert_eq!(
            names,
            [
                "/A",
                "/UPEXT.txt",
                "/UPPER.TXT",
                "/lower.txt",
                "/mixed.TXT",
                "/noext"
            ],
            "{kind:?}"
        );
    }
}

fn root_bytes(image: &[u8], kind: FatKind) -> Vec<u8> {
    let report_geometry = |at: usize| u16::from_le_bytes([image[at], image[at + 1]]) as usize;
    let bps = report_geometry(11);
    let reserved = report_geometry(14);
    let fats = image[16] as usize;
    if kind == FatKind::Fat32 {
        let spf = u32::from_le_bytes(image[36..40].try_into().unwrap()) as usize;
        let start = (reserved + fats * spf) * bps;
        image[start..start + bps * image[13] as usize].to_vec()
    } else {
        let spf = report_geometry(22);
        let entries = report_geometry(17);
        let start = (reserved + fats * spf) * bps;
        image[start..start + entries * 32].to_vec()
    }
}

#[test]
fn short_aliases_get_numeric_tails_and_long_names_store_exactly() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        let names = [
            "The quick brown fox.txt",
            "The quick brown dog.txt",
            "The quick brown cat.txt",
            "Mixed Case.Txt",
            "Readme.md",
            "readme.MD1",
            "Notes.mD1",
            ".hidden",
            "two.dots.in.name",
            "naïve résumé.doc",
            "日本語のファイル.txt",
            "a+b,c;d=e.txt",
        ];
        for name in names {
            volume
                .create_file(name)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        let aliases: Vec<(String, String)> = volume
            .read_dir("/")
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.short_name))
            .collect();
        let alias = |name: &str| {
            aliases
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("{name}"))
                .1
                .clone()
        };
        assert_eq!(alias("The quick brown fox.txt"), "THEQUI~1.TXT");
        assert_eq!(alias("The quick brown dog.txt"), "THEQUI~2.TXT");
        assert_eq!(alias("The quick brown cat.txt"), "THEQUI~3.TXT");
        assert_eq!(alias("Mixed Case.Txt"), "MIXEDC~1.TXT");
        assert_eq!(
            alias("Readme.md"),
            "README.MD",
            "a case difference alone needs no tail"
        );
        assert_eq!(
            alias("readme.MD1"),
            "readme.MD1",
            "one case per part: 8.3 with NT flags"
        );
        assert_eq!(alias("Notes.mD1"), "NOTES.MD1");
        assert_eq!(alias(".hidden"), "HIDDEN~1");
        assert_eq!(alias("two.dots.in.name"), "TWODOT~1.NAM");
        assert_eq!(alias("naïve résumé.doc"), "NA_VER~1.DOC");
        assert_eq!(alias("a+b,c;d=e.txt"), "A_B_C_~1.TXT");
        // They are found by their aliases too.
        assert_eq!(
            volume.stat("THEQUI~2.TXT").unwrap().name,
            "The quick brown dog.txt"
        );
        assert_eq!(
            volume.stat("the quick BROWN cat.TXT").unwrap().short_name,
            "THEQUI~3.TXT"
        );
        let report = closed(volume, &disk);
        for name in names {
            assert!(
                report.tree.contains_key(&format!("/{name}")),
                "{kind:?} {name}"
            );
        }
    }
}

#[test]
fn the_longest_names_and_many_long_entries() {
    let disk = fresh(FatKind::Fat16);
    let mut volume = mount(&disk);
    let longest = "x".repeat(251) + ".txt"; // 255 units
    volume.create_file(&longest).unwrap();
    assert!(matches!(
        volume.create_file(&("y".repeat(252) + ".txt")),
        Err(Error::InvalidName(_))
    ));
    let exact = "abcdefghijklm"; // 13 units: exactly one long entry, no terminator
    volume.create_file(exact).unwrap();
    let two = "abcdefghijklmnopqrstuvwxyz"; // 26: two entries, no terminator
    volume.create_file(two).unwrap();
    let report = closed(volume, &disk);
    for name in [longest.as_str(), exact, two] {
        assert!(report.tree.contains_key(&format!("/{name}")), "{name}");
    }
}

#[test]
fn bad_names_are_refused_before_anything_is_written() {
    let disk = fresh(FatKind::Fat32);
    let mut volume = mount(&disk);
    volume.create_file("ok.txt").unwrap();
    let before = disk.bytes();
    for name in [
        "a:b", "a*b", "a?b", "a<b", "a>b", "a|b", "a\"b", "trail.", "trail ", "a\tb", "a\\b",
    ] {
        assert!(
            matches!(volume.create_file(name), Err(Error::InvalidName(_))),
            "{name:?}"
        );
    }
    assert_eq!(disk.bytes(), before);
    closed(volume, &disk);
}

#[test]
fn overwrite_append_and_sparse_writes() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        let file = volume.create_file("data.bin").unwrap();
        let mut expected = pattern(1500, 3);
        volume.write(&file, 0, &expected).unwrap();
        // Overwrite across a cluster boundary.
        let patch = pattern(300, 99);
        volume.write(&file, 400, &patch).unwrap();
        expected[400..700].copy_from_slice(&patch);
        // Append.
        let tail = pattern(900, 7);
        volume.write(&file, 1500, &tail).unwrap();
        expected.extend_from_slice(&tail);
        // A write far past the end leaves a hole that reads as zeros.
        let far = pattern(10, 1);
        volume.write(&file, 5000, &far).unwrap();
        expected.resize(5000, 0);
        expected.extend_from_slice(&far);
        assert_eq!(read_all(&volume, &file), expected, "{kind:?}");
        assert_eq!(volume.len(&file).unwrap(), 5010);
        // A zero-length write changes nothing, even past the end.
        assert_eq!(volume.write(&file, 9999, &[]).unwrap(), 0);
        assert_eq!(volume.len(&file).unwrap(), 5010);
        let report = closed(volume, &disk);
        assert_eq!(report.tree["/data.bin"].data, expected, "{kind:?}");
    }
}

#[test]
fn growing_never_exposes_what_an_earlier_file_left_in_the_clusters() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        let secret = volume.create_file("secret.bin").unwrap();
        volume.write(&secret, 0, &vec![0xA5; 4000]).unwrap();
        volume.unlink("secret.bin").unwrap();
        // The new file reuses the freed clusters.
        let file = volume.create_file("new.bin").unwrap();
        volume.write(&file, 3000, b"end").unwrap();
        let data = read_all(&volume, &file);
        assert!(
            data[..3000].iter().all(|b| *b == 0),
            "{kind:?}: gap is zero"
        );
        // Shrinking then growing again also reads zeros, not the old tail.
        volume.write(&file, 0, &vec![0x77; 3000]).unwrap();
        volume.truncate(&file, 700).unwrap();
        volume.truncate(&file, 2500).unwrap();
        let data = read_all(&volume, &file);
        assert!(data[..700].iter().all(|b| *b == 0x77));
        assert!(
            data[700..].iter().all(|b| *b == 0),
            "{kind:?}: grown part is zero"
        );
        let report = closed(volume, &disk);
        assert_eq!(report.tree["/new.bin"].size, 2500);
    }
}

#[test]
fn truncate_frees_and_grows_and_unlink_returns_every_cluster() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        let initial = volume.free_clusters();
        let file = volume.create_file("t.bin").unwrap();
        volume.write(&file, 0, &pattern(10_000, 5)).unwrap();
        let used = initial - volume.free_clusters();
        assert_eq!(used, 20); // 10000 / 512, rounded up
        volume.truncate(&file, 5000).unwrap();
        assert_eq!(initial - volume.free_clusters(), 10);
        volume.truncate(&file, 0).unwrap();
        assert_eq!(volume.free_clusters(), initial);
        assert_eq!(volume.stat("t.bin").unwrap().first_cluster, 0);
        volume.truncate(&file, 1025).unwrap();
        assert_eq!(initial - volume.free_clusters(), 3);
        assert_eq!(read_all(&volume, &file), vec![0u8; 1025]);
        volume.write(&file, 0, b"x").unwrap();
        volume.truncate(&file, 1025).unwrap(); // no change
        volume.unlink("t.bin").unwrap();
        assert_eq!(volume.free_clusters(), initial);
        let report = closed(volume, &disk);
        assert!(report.tree.is_empty());
        assert_eq!(report.free_clusters, initial);
    }
}

#[test]
fn unlink_and_rmdir_refuse_the_wrong_kind_and_a_non_empty_directory() {
    let disk = fresh(FatKind::Fat32);
    let mut volume = mount(&disk);
    volume.mkdir("d").unwrap();
    volume.create_file("d/f").unwrap();
    volume.create_file("file").unwrap();
    assert!(matches!(volume.unlink("d"), Err(Error::IsADirectory)));
    assert!(matches!(volume.rmdir("file"), Err(Error::NotADirectory)));
    assert!(matches!(volume.rmdir("d"), Err(Error::NotEmpty)));
    assert!(matches!(volume.unlink("missing"), Err(Error::NotFound)));
    assert!(matches!(volume.rmdir("missing"), Err(Error::NotFound)));
    assert!(matches!(volume.rmdir("/"), Err(Error::InvalidName(_))));
    volume.unlink("d/f").unwrap();
    volume.rmdir("d").unwrap();
    let report = closed(volume, &disk);
    assert_eq!(contents(&report), model(&[("/file", Some(vec![]))]));
}

#[test]
fn rename_in_a_directory_across_directories_and_for_directories() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        volume.mkdir("a").unwrap();
        volume.mkdir("b").unwrap();
        volume.mkdir("a/sub").unwrap();
        let file = volume.create_file("a/one.txt").unwrap();
        volume.write(&file, 0, &pattern(2000, 1)).unwrap();
        let inner = volume.create_file("a/sub/inner.txt").unwrap();
        volume.write(&inner, 0, b"inner").unwrap();
        // Within a directory, to a name that needs a long entry.
        volume
            .rename("a/one.txt", "a/A much longer name now.txt")
            .unwrap();
        // Across directories.
        volume
            .rename("a/A much longer name now.txt", "b/short.txt")
            .unwrap();
        // A directory with contents moves and its `..` follows.
        volume.rename("a/sub", "b/moved sub").unwrap();
        // Case only.
        volume.rename("b/short.txt", "b/SHORT.TXT").unwrap();
        assert_eq!(volume.stat("b/short.txt").unwrap().name, "SHORT.TXT");
        // To the root and back.
        volume.rename("b/moved sub", "rootsub").unwrap();
        let report = closed(volume, &disk);
        assert_eq!(
            contents(&report),
            model(&[
                ("/a", None),
                ("/b", None),
                ("/b/SHORT.TXT", Some(pattern(2000, 1))),
                ("/rootsub", None),
                ("/rootsub/inner.txt", Some(b"inner".to_vec())),
            ]),
            "{kind:?}"
        );
    }
}

#[test]
fn rename_refuses_an_existing_target_and_a_directory_into_itself() {
    let disk = fresh(FatKind::Fat16);
    let mut volume = mount(&disk);
    volume.mkdir("d").unwrap();
    volume.mkdir("d/e").unwrap();
    volume.create_file("x").unwrap();
    volume.create_file("y").unwrap();
    assert!(matches!(volume.rename("x", "y"), Err(Error::Exists)));
    assert!(matches!(volume.rename("x", "Y"), Err(Error::Exists)));
    assert!(matches!(volume.rename("x", "d"), Err(Error::Exists)));
    assert!(matches!(
        volume.rename("d", "d/e/inside"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        volume.rename("d", "d/inside"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        volume.rename("nothing", "z"),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        volume.rename("x", "nodir/z"),
        Err(Error::NotFound)
    ));
    volume.rename("x", "x").unwrap();
    closed(volume, &disk);
}

#[test]
fn a_directory_grows_past_one_cluster_and_deleted_slots_are_reused() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        volume.mkdir("many").unwrap();
        // One 512-byte cluster holds 16 entries; each of these needs 3.
        let count = 40;
        for index in 0..count {
            volume
                .create_file(&format!("many/a file with a long name {index:03}.dat"))
                .unwrap();
        }
        let before = volume.free_clusters();
        for index in (0..count).step_by(2) {
            volume
                .unlink(&format!("many/a file with a long name {index:03}.dat"))
                .unwrap();
        }
        // Freed slots are filled before the directory grows again.
        let grown_after = volume.free_clusters();
        for index in 0..count / 2 {
            volume
                .create_file(&format!("many/replacement name number {index:03}.dat"))
                .unwrap();
        }
        assert_eq!(
            volume.free_clusters(),
            grown_after,
            "{kind:?}: no growth needed"
        );
        assert!(before <= grown_after);
        assert_eq!(volume.read_dir("many").unwrap().len(), count);
        let report = closed(volume, &disk);
        assert_eq!(report.tree.len(), count + 1);
    }
}

#[test]
fn a_full_fixed_root_directory_is_refused_and_nothing_changes() {
    for kind in [FatKind::Fat12, FatKind::Fat16] {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        let mut made = 0usize;
        let error = loop {
            match volume.create_file(&format!("F{made:05}")) {
                Ok(_) => made += 1,
                Err(error) => break error,
            }
        };
        assert!(
            matches!(error, Error::RootDirectoryFull),
            "{kind:?}: {error}"
        );
        let entries = if kind == FatKind::Fat12 { 224 } else { 512 };
        assert_eq!(made, entries, "{kind:?}: every root slot is usable");
        let before = disk.bytes();
        assert!(matches!(
            volume.create_file("one more"),
            Err(Error::RootDirectoryFull)
        ));
        assert!(matches!(volume.mkdir("dir"), Err(Error::RootDirectoryFull)));
        assert_eq!(
            disk.bytes(),
            before,
            "{kind:?}: a refused create writes nothing"
        );
        // Subdirectories are not limited by the fixed root.
        let free = volume.free_clusters();
        volume.unlink("F00000").unwrap();
        volume.mkdir("sub").unwrap();
        volume.create_file("sub/inside").unwrap();
        assert!(volume.free_clusters() < free + 1);
        closed(volume, &disk);
    }
}

#[test]
fn a_full_volume_is_refused_cleanly_and_the_volume_stays_consistent() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        let small = volume.create_file("small.txt").unwrap();
        volume.write(&small, 0, b"keep me").unwrap();
        let big = volume.create_file("big.bin").unwrap();
        let chunk = pattern(64 * 1024, 11);
        let mut written = 0u64;
        let error = loop {
            match volume.write(&big, written, &chunk) {
                Ok(n) => written += n as u64,
                Err(error) => break error,
            }
        };
        assert!(matches!(error, Error::VolumeFull), "{kind:?}: {error}");
        // The failed write changed nothing: the file has what was written.
        assert_eq!(volume.len(&big).unwrap(), written);
        let free = volume.free_clusters();
        let before = disk.bytes();
        // Fewer than a chunk may still fit; fill the rest a cluster at a time.
        for _ in 0..free {
            volume.write(&big, written, &[1u8; 512]).unwrap();
            written += 512;
        }
        assert_eq!(volume.free_clusters(), 0);
        let before_full = disk.bytes();
        assert_ne!(before, before_full);
        assert!(matches!(
            volume.write(&big, written, b"x"),
            Err(Error::VolumeFull)
        ));
        assert!(matches!(volume.mkdir("nospace"), Err(Error::VolumeFull)));
        assert!(matches!(
            volume.truncate(&big, written + 1),
            Err(Error::VolumeFull)
        ));
        // Creating an empty file needs no cluster, as long as the directory has room.
        volume.create_file("empty.txt").unwrap();
        let after = disk.bytes();
        assert!(after != before_full);
        // The refused operations wrote nothing but the empty file's entry.
        volume.unlink("big.bin").unwrap();
        let report = closed(volume, &disk);
        assert_eq!(report.tree["/small.txt"].data, b"keep me");
        assert_eq!(
            report.free_clusters,
            report.cluster_count - u32::from(kind == FatKind::Fat32) - 1
        );
    }
}

#[test]
fn fat_state_is_unchanged_by_every_refused_operation() {
    let disk = fresh(FatKind::Fat12);
    let mut volume = mount(&disk);
    let file = volume.create_file("f").unwrap();
    volume.write(&file, 0, &pattern(3000, 2)).unwrap();
    let before = disk.bytes();
    assert!(matches!(
        volume.truncate(&file, u64::from(u32::MAX) + 1),
        Err(Error::FileTooLarge)
    ));
    assert!(matches!(
        volume.write(&file, u64::from(u32::MAX), b"xx"),
        Err(Error::FileTooLarge)
    ));
    assert!(matches!(
        volume.write(&file, u64::MAX, b"x"),
        Err(Error::FileTooLarge)
    ));
    assert!(matches!(volume.mkdir("f"), Err(Error::Exists)));
    assert!(matches!(volume.open("nothing"), Err(Error::NotFound)));
    assert!(matches!(volume.read_dir("f"), Err(Error::NotADirectory)));
    assert_eq!(disk.bytes(), before);
    closed(volume, &disk);
}

#[test]
fn fat12_packed_entries_survive_chains_across_sector_boundaries() {
    let disk = fresh(FatKind::Fat12);
    let mut volume = mount(&disk);
    // Small files first so later chains cross cluster 341 (a FAT sector
    // boundary) and the odd/even packing in every position.
    let mut expected = Vec::new();
    for index in 0..60 {
        let name = format!("p{index:02}.bin");
        let data = pattern(512 * (1 + index % 9) + index, index as u8);
        let handle = volume.create_file(&name).unwrap();
        volume.write(&handle, 0, &data).unwrap();
        expected.push((format!("/{name}"), Some(data)));
    }
    for index in (0..60).step_by(3) {
        volume.unlink(&format!("p{index:02}.bin")).unwrap();
        expected[index].1 = None;
    }
    let big = volume.create_file("big.bin").unwrap();
    volume.write(&big, 0, &pattern(100_000, 4)).unwrap();
    expected.push(("/big.bin".to_string(), Some(pattern(100_000, 4))));
    let report = closed(volume, &disk);
    let want: BTreeMap<_, _> = expected.into_iter().filter(|(_, d)| d.is_some()).collect();
    assert_eq!(contents(&report), want);
    // At least one file's chain is fragmented over the freed holes.
    let big = &report.tree["/big.bin"];
    assert!(big.first >= 2);
}

#[test]
fn files_fragment_over_freed_holes_and_read_back() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        let mut keep = Vec::new();
        for index in 0..30 {
            let handle = volume.create_file(&format!("s{index:02}")).unwrap();
            volume
                .write(&handle, 0, &pattern(1024, index as u8))
                .unwrap();
            keep.push(handle);
        }
        for index in (0..30).step_by(2) {
            volume.unlink(&format!("s{index:02}")).unwrap();
        }
        // The hole at the front is two clusters; this file needs fifteen
        // holes' worth, so it is fragmented.
        let frag = volume.create_file("frag.bin").unwrap();
        let data = pattern(512 * 20, 77);
        // Make the allocator start at the front again.
        volume.write(&frag, 0, &data).unwrap();
        assert_eq!(read_all(&volume, &frag), data);
        let report = closed(volume, &disk);
        assert_eq!(report.tree["/frag.bin"].data, data, "{kind:?}");
        assert_eq!(report.tree.len(), 16, "{kind:?}");
    }
}

#[test]
fn handles_belong_to_their_volume_and_their_directory_entry() {
    let disk = fresh(FatKind::Fat16);
    let other_disk = fresh(FatKind::Fat16);
    let mut volume = mount(&disk);
    let mut other = mount(&other_disk);
    let file = volume.create_file("a.txt").unwrap();
    volume.write(&file, 0, b"first").unwrap();
    // Another volume's handle is refused.
    let foreign = other.create_file("a.txt").unwrap();
    assert!(matches!(
        volume.write(&foreign, 0, b"x"),
        Err(Error::ForeignHandle)
    ));
    assert!(matches!(
        volume.read(&foreign, 0, &mut [0u8; 1]),
        Err(Error::ForeignHandle)
    ));
    assert!(matches!(volume.len(&foreign), Err(Error::ForeignHandle)));
    // A second handle to the same file stays valid alongside the first.
    let again = volume.open("a.txt").unwrap();
    assert_eq!(read_all(&volume, &again), b"first");
    // Removing the file makes both stale, and a new file in the recycled
    // slot is a different file that the old handles cannot reach.
    volume.unlink("a.txt").unwrap();
    let replacement = volume.create_file("a.txt").unwrap();
    volume.write(&replacement, 0, b"second").unwrap();
    for stale in [&file, &again] {
        assert!(matches!(
            volume.write(stale, 0, b"X"),
            Err(Error::StaleHandle)
        ));
        assert!(matches!(
            volume.read(stale, 0, &mut [0u8; 1]),
            Err(Error::StaleHandle)
        ));
        assert!(matches!(volume.truncate(stale, 0), Err(Error::StaleHandle)));
    }
    assert_eq!(read_all(&volume, &replacement), b"second");
    // Another name in the same slot is stale too.
    volume.unlink("a.txt").unwrap();
    let different = volume.create_file("b.txt").unwrap();
    assert!(matches!(
        volume.write(&replacement, 0, b"X"),
        Err(Error::StaleHandle)
    ));
    assert_eq!(volume.len(&different).unwrap(), 0);
    // A renamed file's old handle is stale; the new name opens afresh.
    volume.rename("b.txt", "c.txt").unwrap();
    assert!(matches!(volume.len(&different), Err(Error::StaleHandle)));
    let renamed = volume.open("c.txt").unwrap();
    assert_eq!(volume.len(&renamed).unwrap(), 0);
    closed(volume, &disk);
    closed(other, &other_disk);
}

#[test]
fn a_handle_cannot_reach_a_file_in_a_recycled_directory() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        volume.mkdir("d").unwrap();
        let inner = volume.create_file("d/f.txt").unwrap();
        volume.write(&inner, 0, b"old").unwrap();
        volume.unlink("d/f.txt").unwrap();
        volume.rmdir("d").unwrap();
        // The new directory reuses the freed cluster and its first slot.
        volume.mkdir("e").unwrap();
        let fresh_file = volume.create_file("e/f.txt").unwrap();
        volume.write(&fresh_file, 0, b"new").unwrap();
        assert!(
            matches!(
                volume.read(&inner, 0, &mut [0u8; 3]),
                Err(Error::StaleHandle)
            ),
            "{kind:?}"
        );
        assert!(matches!(
            volume.write(&inner, 0, b"zzz"),
            Err(Error::StaleHandle)
        ));
        assert_eq!(read_all(&volume, &fresh_file), b"new");
        closed(volume, &disk);
    }
}

#[test]
fn directories_cannot_be_opened_as_files() {
    let disk = fresh(FatKind::Fat32);
    let mut volume = mount(&disk);
    volume.mkdir("d").unwrap();
    assert!(matches!(volume.open("d"), Err(Error::IsADirectory)));
    closed(volume, &disk);
}

#[test]
fn timestamps_are_stamped_from_the_clock() {
    let disk = fresh(FatKind::Fat32);
    let mut volume = mount(&disk);
    let file = volume.create_file("when.txt").unwrap();
    let info = volume.stat("when.txt").unwrap();
    assert_eq!(info.created, clock());
    assert_eq!(info.modified.date, clock().date);
    assert_eq!(info.modified.time, clock().time);
    assert_eq!(info.accessed_date, clock().date);
    volume.set_clock(later);
    volume.write(&file, 0, b"changed").unwrap();
    let info = volume.stat("when.txt").unwrap();
    assert_eq!(info.created, clock(), "creation time does not move");
    assert_eq!(info.modified.date, later().date);
    assert_eq!(info.modified.time, later().time);
    assert_eq!(info.accessed_date, later().date);
    assert_eq!(info.modified.to_unix().unwrap().0, 1_751_327_998);
    volume.mkdir("dir").unwrap();
    assert_eq!(volume.stat("dir").unwrap().created.date, later().date);
    closed(volume, &disk);
}

#[test]
fn fsinfo_counts_and_hints_follow_every_change() {
    let disk = fresh(FatKind::Fat32);
    let mut volume = mount(&disk);
    let info = |disk: &Disk| {
        let bytes = disk.bytes();
        let at = |o: usize| u32::from_le_bytes(bytes[512 + o..512 + o + 4].try_into().unwrap());
        (at(488), at(492))
    };
    let (free0, hint0) = info(&disk);
    assert_eq!(free0, volume.free_clusters());
    assert_eq!(hint0, 3);
    let file = volume.create_file("f").unwrap();
    volume.write(&file, 0, &pattern(5000, 1)).unwrap();
    let (free1, hint1) = info(&disk);
    assert_eq!(free1, volume.free_clusters());
    assert_eq!(free1, free0 - 10);
    assert_eq!(hint1, 13);
    volume.unlink("f").unwrap();
    assert_eq!(info(&disk).0, free0);
    closed(volume, &disk);
}

#[test]
fn a_damaged_fsinfo_is_left_alone_and_a_wrong_count_is_corrected() {
    let disk = fresh(FatKind::Fat32);
    // Unknown free count (as other tools write it) is replaced by the real one.
    disk.patch(512 + 488, &0xFFFF_FFFFu32.to_le_bytes());
    let mut volume = mount(&disk);
    let file = volume.create_file("f").unwrap();
    volume.write(&file, 0, &[1; 1000]).unwrap();
    let report = closed(volume, &disk);
    assert!(report.free_clusters > 0);

    // A sector that is not an FSInfo is not written to.
    let disk = fresh(FatKind::Fat32);
    disk.patch(512, &[0u8; 4]);
    let before = disk.bytes()[512..1024].to_vec();
    let mut volume = mount(&disk);
    let file = volume.create_file("f").unwrap();
    volume.write(&file, 0, &[1; 1000]).unwrap();
    volume.unmount().unwrap();
    assert_eq!(disk.bytes()[512..1024], before[..]);
}

#[test]
fn operations_on_a_volume_inside_a_larger_device() {
    let offset = 1024 * 1024;
    let inner = fresh(FatKind::Fat16).bytes();
    let mut bytes = vec![0xEE; offset];
    bytes.extend_from_slice(&inner);
    bytes.extend(vec![0xEE; 4096]);
    let disk = Disk::new(bytes);
    let mut volume = open_writable(disk.clone(), offset as u64).unwrap();
    volume.mkdir("d").unwrap();
    let file = volume.create_file("d/x.bin").unwrap();
    volume.write(&file, 0, &pattern(20_000, 9)).unwrap();
    volume.unmount().unwrap();
    let after = disk.bytes();
    assert!(after[..offset].iter().all(|b| *b == 0xEE));
    assert!(after[offset + inner.len()..].iter().all(|b| *b == 0xEE));
    let report = check(&after[offset..offset + inner.len()], true);
    assert_eq!(report.tree["/d/x.bin"].data, pattern(20_000, 9));
}

#[test]
fn a_dirty_volume_is_marked_while_operations_run_and_clean_after() {
    for kind in [FatKind::Fat16, FatKind::Fat32] {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        volume.mkdir("d").unwrap();
        // Mid-operation the independent check sees the dirty marks.
        let report = check(&disk.bytes(), false);
        assert_eq!(report.tree.len(), 1);
        volume.unmount().unwrap();
        check(&disk.bytes(), true);
    }
}

/// A small deterministic generator, so a failure repeats.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn a_long_random_sequence_agrees_with_a_model_and_the_independent_check() {
    for kind in KINDS {
        let disk = fresh(kind);
        let mut volume = mount(&disk);
        let mut rng = Rng(0xFA7 + kind as u64);
        // Model: path -> Some(contents) for files, None for directories.
        let mut files: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
        let names = [
            "alpha",
            "Beta.txt",
            "gamma delta.dat",
            "EPSILON.BIN",
            "zeta-zeta-zeta-zeta.log",
            "eta.x",
            "theta theta.tar.gz",
            "iota",
            "kappa.md",
            "lambda lambda lambda.txt",
        ];
        for step in 0..400 {
            let dirs: Vec<String> = std::iter::once(String::new())
                .chain(
                    files
                        .iter()
                        .filter(|(_, v)| v.is_none())
                        .map(|(k, _)| k[1..].to_string()),
                )
                .collect();
            let dir = dirs[rng.below(dirs.len())].clone();
            let name = names[rng.below(names.len())];
            let path = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{dir}/{name}")
            };
            let key = format!("/{path}");
            let exists = files.keys().any(|k| k.to_lowercase() == key.to_lowercase());
            let existing_key = files
                .keys()
                .find(|k| k.to_lowercase() == key.to_lowercase())
                .cloned();
            match rng.below(8) {
                0 | 1 if !exists => {
                    volume.create_file(&path).unwrap();
                    files.insert(key, Some(Vec::new()));
                }
                2 if !exists && dir.matches('/').count() < 2 => {
                    volume.mkdir(&path).unwrap();
                    files.insert(key, None);
                }
                3 | 4 if exists && files[existing_key.as_ref().unwrap()].is_some() => {
                    let k = existing_key.unwrap();
                    let handle = volume.open(&k[1..]).unwrap();
                    let data = files.get_mut(&k).unwrap().as_mut().unwrap();
                    let offset = rng.below(data.len() + 600);
                    let chunk = pattern(rng.below(2500) + 1, step as u8);
                    volume.write(&handle, offset as u64, &chunk).unwrap();
                    if data.len() < offset + chunk.len() {
                        data.resize(offset + chunk.len(), 0);
                    }
                    data[offset..offset + chunk.len()].copy_from_slice(&chunk);
                }
                5 if exists && files[existing_key.as_ref().unwrap()].is_some() => {
                    let k = existing_key.unwrap();
                    let handle = volume.open(&k[1..]).unwrap();
                    let data = files.get_mut(&k).unwrap().as_mut().unwrap();
                    let length = rng.below(data.len() + 1500);
                    volume.truncate(&handle, length as u64).unwrap();
                    data.resize(length, 0);
                }
                6 if exists => {
                    let k = existing_key.unwrap();
                    let is_dir = files[&k].is_none();
                    let has_children = files.keys().any(|o| {
                        o.to_lowercase()
                            .starts_with(&format!("{}/", k.to_lowercase()))
                    });
                    if is_dir && has_children {
                        assert!(matches!(volume.rmdir(&k[1..]), Err(Error::NotEmpty)));
                    } else if is_dir {
                        volume.rmdir(&k[1..]).unwrap();
                        files.remove(&k);
                    } else {
                        volume.unlink(&k[1..]).unwrap();
                        files.remove(&k);
                    }
                }
                7 if exists && files[existing_key.as_ref().unwrap()].is_some() => {
                    // Rename a file within its directory to another name.
                    let k = existing_key.unwrap();
                    let new_name = names[rng.below(names.len())];
                    let new_path = if dir.is_empty() {
                        new_name.to_string()
                    } else {
                        format!("{dir}/{new_name}")
                    };
                    let new_key = format!("/{new_path}");
                    let taken = files
                        .keys()
                        .any(|o| o.to_lowercase() == new_key.to_lowercase());
                    if !taken {
                        volume.rename(&k[1..], &new_path).unwrap();
                        let data = files.remove(&k).unwrap();
                        files.insert(new_key, data);
                    }
                }
                _ => {}
            }
            if step % 40 == 39 {
                let report = check(
                    &{
                        // The volume is open and marked dirty: check the structure.
                        disk.bytes()
                    },
                    false,
                );
                assert_eq!(contents(&report), files, "{kind:?} step {step}");
            }
        }
        let report = closed(volume, &disk);
        assert_eq!(contents(&report), files, "{kind:?}");
    }
}

#[test]
fn a_large_file_written_in_big_pieces_reads_back() {
    let disk = fresh(FatKind::Fat32);
    let mut volume = mount(&disk);
    let file = volume.create_file("large.bin").unwrap();
    let total = 20 * MIB;
    let started = std::time::Instant::now();
    for index in 0..total / MIB {
        volume
            .write(&file, (index * MIB) as u64, &pattern(MIB, index as u8))
            .unwrap();
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "writing 20 MiB took {:?}",
        started.elapsed()
    );
    let mut sample = vec![0u8; 4096];
    volume
        .read(&file, (7 * MIB + 100) as u64, &mut sample)
        .unwrap();
    assert_eq!(sample, pattern(MIB, 7)[100..4196]);
    let report = closed(volume, &disk);
    let data = &report.tree["/large.bin"].data;
    assert_eq!(data.len(), total);
    for index in 0..total / MIB {
        assert_eq!(
            data[index * MIB..(index + 1) * MIB],
            pattern(MIB, index as u8)[..],
            "{index}"
        );
    }
}

#[test]
fn an_image_file_is_formatted_written_and_checked_through_the_path_constructor() {
    use fs_fat32::mkfs::format_path;
    use fs_fat32::write::open_writable_path;
    let path = std::env::temp_dir().join(format!("fat-write-ops-{}.img", std::process::id()));
    let _ = std::fs::remove_file(&path);
    format_path(
        &path,
        &FormatOptions {
            size: Some(8 * MIB as u64),
            kind: Some(FatKind::Fat16),
            ..FormatOptions::default()
        },
    )
    .unwrap();
    let mut volume = open_writable_path(&path, 0).unwrap();
    volume.mkdir("photos").unwrap();
    let file = volume.create_file("photos/IMG 0001.jpg").unwrap();
    volume.write(&file, 0, &pattern(123_456, 3)).unwrap();
    volume.unmount().unwrap();
    let image = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let report = check(&image, true);
    assert_eq!(
        report.tree["/photos/IMG 0001.jpg"].data,
        pattern(123_456, 3)
    );
}
