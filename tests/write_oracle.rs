//! What the driver writes, checked by implementations that are not the driver.
//!
//! Every image is made by `fs_fat32::mkfs` and changed by `WriteVolume`, then
//! closed cleanly and handed to three independent implementations
//! (`validation/inspect-image.sh`, in the Linux test VM or natively on a Linux
//! runner):
//!
//! - `fsck.fat -n` from dosfstools must report it clean, after each operation;
//! - the Linux kernel's vfat driver must mount it read-only and show the same
//!   names, sizes, times and contents as the driver does;
//! - mtools (`mcopy`, `mdir`) must read it and copy out the same files.
//!
//! The reverse direction is covered too: volumes built and filled by the kernel
//! (`validation/generate.sh`) are read back by the driver, then changed by it
//! and checked again. Damage recipes prove the oracles can fail.
//!
//! Needs the oracles, so it is behind the `oracle` feature and FAILS (never
//! skips) when they cannot run.
#![cfg(feature = "oracle")]

mod common;
mod oracle;
mod support;

use oracle::scenario::{kind_name, mount, options, pattern, scenario, view, View, MIB};

use fs_fat32::mkfs::format_path;
use fs_fat32::timestamp::FatTimestamp;
use fs_fat32::{Error, FatKind};
use oracle::{inspect_images, work_dir, Inspect};
use std::path::{Path, PathBuf};

const WORK: &str = "fat-write";

/// The driver's view against what the kernel and mtools report.
fn agree(name: &str, view: &View, inspect: &Inspect, tools: &str) {
    assert_eq!(
        inspect.fsck_exit, 0,
        "{name}: fsck.fat -n is not clean:\n{}\n{tools}",
        inspect.fsck_output
    );
    assert!(
        inspect.kernel_mounted,
        "{name}: the kernel refused it: {}",
        inspect.mount_error
    );
    assert_eq!(
        inspect.mtools_exit, 0,
        "{name}: mcopy failed: {}",
        inspect.mtools_error
    );
    assert_eq!(inspect.mdir_exit, 0, "{name}: mdir failed");
    let ours: Vec<&String> = view.keys().collect();
    let kernel: Vec<&String> = inspect.kernel.keys().collect();
    let mtools: Vec<&String> = inspect.mtools.keys().collect();
    assert_eq!(ours, kernel, "{name}: names, driver against kernel");
    assert_eq!(ours, mtools, "{name}: names, driver against mtools");
    for (path, (info, sha)) in view {
        let entry = &inspect.kernel[path];
        assert_eq!(info.is_dir, entry.dir, "{name}: {path}: directory");
        let copied = &inspect.mtools[path];
        assert_eq!(
            info.is_dir, copied.dir,
            "{name}: {path}: directory (mtools)"
        );
        if !info.is_dir {
            assert_eq!(u64::from(info.size), entry.size, "{name}: {path}: size");
            assert_eq!(
                sha, &entry.sha,
                "{name}: {path}: contents, driver against kernel"
            );
            assert_eq!(
                u64::from(info.size),
                copied.size,
                "{name}: {path}: size (mtools)"
            );
            assert_eq!(
                sha, &copied.sha,
                "{name}: {path}: contents, driver against mtools"
            );
        }
        let (modified, _) = info.modified.to_unix().expect("a real modification time");
        assert_eq!(
            entry.mtime.floor() as i64,
            modified,
            "{name}: {path}: modification time"
        );
        assert_eq!(
            copied.mtime, modified,
            "{name}: {path}: modification time (mtools)"
        );
        let (created, millis) = info.created.to_unix().expect("a real creation time");
        let created = created as f64 + f64::from(millis) / 1000.0;
        assert!(
            (entry.ctime - created).abs() < 0.011,
            "{name}: {path}: creation time {} against {created}",
            entry.ctime
        );
        let day = FatTimestamp {
            date: info.accessed_date,
            time: 0,
            tenths: 0,
        };
        assert_eq!(
            entry.atime,
            day.to_unix().unwrap().0,
            "{name}: {path}: access date"
        );
    }
}

fn run_scenario(tag: &str, kind: FatKind) -> Vec<(String, PathBuf)> {
    let dir = work_dir(WORK);
    let (opts, _) = options(kind);
    let path = dir.join(format!("{tag}-{}-live.img", kind_name(kind)));
    let _ = std::fs::remove_file(&path);
    format_path(&path, &opts).unwrap();
    let mut snapshots = Vec::new();
    // The freshly formatted volume is the first thing checked.
    let first = dir.join(format!("{tag}-{}-00-formatted.img", kind_name(kind)));
    std::fs::copy(&path, &first).unwrap();
    snapshots.push(("formatted".to_string(), first));
    for (index, step) in scenario().iter().enumerate() {
        let mut volume = mount(&path, 0);
        (step.run)(&mut volume);
        volume.unmount().unwrap();
        let snapshot = dir.join(format!(
            "{}-{:02}-{}.img",
            kind_name(kind),
            index + 1,
            step.name
        ));
        std::fs::copy(&path, &snapshot).unwrap();
        snapshots.push((step.name.to_string(), snapshot));
    }
    snapshots
}

fn check_all(label: &str, items: &[(String, PathBuf)]) {
    let images: Vec<PathBuf> = items.iter().map(|(_, p)| p.clone()).collect();
    let (inspects, tools) = inspect_images(WORK, &images);
    eprintln!("tools:\n{tools}");
    for ((name, path), inspect) in items.iter().zip(&inspects) {
        agree(
            &format!("{label} after {name}"),
            &view(path, 0),
            inspect,
            &tools,
        );
    }
}

#[test]
fn fat12_after_every_operation() {
    check_all("fat12", &run_scenario("ops", FatKind::Fat12));
}

#[test]
fn fat16_after_every_operation() {
    check_all("fat16", &run_scenario("ops", FatKind::Fat16));
}

#[test]
fn fat32_after_every_operation() {
    check_all("fat32", &run_scenario("ops", FatKind::Fat32));
}

#[test]
fn short_aliases_are_the_ones_the_kernel_and_mtools_would_make_for_readers() {
    // The kernel and mtools read these names back by their long names, and
    // fsck.fat accepts the ~N aliases: this pins the alias text as well.
    let dir = work_dir(WORK);
    let path = dir.join("aliases.img");
    let _ = std::fs::remove_file(&path);
    let (opts, _) = options(FatKind::Fat16);
    format_path(&path, &opts).unwrap();
    let mut volume = mount(&path, 0);
    for name in [
        "The quick brown fox.txt",
        "The quick brown dog.txt",
        "The quick brown cat.txt",
    ] {
        volume.create_file(name).unwrap();
    }
    let aliases: Vec<String> = volume
        .read_dir("")
        .unwrap()
        .into_iter()
        .map(|e| e.short_name)
        .collect();
    volume.unmount().unwrap();
    assert_eq!(aliases, ["THEQUI~1.TXT", "THEQUI~2.TXT", "THEQUI~3.TXT"]);
    check_all("aliases", &[("three colliding names".to_string(), path)]);
}

/// Fills a small volume until it is full, and a fixed root until it is full,
/// and checks the volumes left behind.
fn fill_scenarios(kind: FatKind) -> Vec<(String, PathBuf)> {
    let dir = work_dir(WORK);
    let mut out = Vec::new();
    // A full volume.
    let (mut opts, _) = options(kind);
    opts.size = Some(if kind == FatKind::Fat32 {
        34 * MIB
    } else if kind == FatKind::Fat16 {
        4 * MIB
    } else {
        720 * 1024
    });
    if kind == FatKind::Fat16 {
        opts.sectors_per_cluster = Some(1);
    }
    let path = dir.join(format!("{}-full-volume.img", kind_name(kind)));
    let _ = std::fs::remove_file(&path);
    format_path(&path, &opts).unwrap();
    let mut volume = mount(&path, 0);
    volume.mkdir("files").unwrap();
    let mut index = 0;
    loop {
        let name = format!("files/file {index:04}.bin");
        match volume.create_file(&name) {
            Ok(handle) => match volume.write(&handle, 0, &pattern(150_000, index as u8)) {
                Ok(_) => index += 1,
                Err(Error::VolumeFull) => {
                    // Fill what is left with a final file, to the last cluster.
                    let left = u64::from(volume.free_clusters()) * volume.boot().cluster_size();
                    if left > 0 {
                        volume
                            .write(&handle, 0, &pattern(left as usize, 7))
                            .unwrap();
                    }
                    break;
                }
                Err(error) => panic!("{error}"),
            },
            Err(Error::VolumeFull) => break,
            Err(error) => panic!("{error}"),
        }
    }
    assert_eq!(volume.free_clusters(), 0);
    assert!(matches!(volume.mkdir("nospace"), Err(Error::VolumeFull)));
    volume.unmount().unwrap();
    out.push(("a full volume".to_string(), path));
    // A full fixed root directory (FAT12 and FAT16), then room made again.
    if kind != FatKind::Fat32 {
        let (mut opts, _) = options(kind);
        opts.size = Some(if kind == FatKind::Fat16 {
            4 * MIB
        } else {
            1440 * 1024
        });
        opts.sectors_per_cluster = Some(1);
        let path = dir.join(format!("{}-full-root.img", kind_name(kind)));
        let _ = std::fs::remove_file(&path);
        let layout = format_path(&path, &opts).unwrap();
        let mut volume = mount(&path, 0);
        let mut made = 0;
        while volume.create_file(&format!("R{made:05}.DAT")).is_ok() {
            made += 1;
        }
        // The label entry takes one slot of the fixed root.
        assert_eq!(made, layout.root_entries as usize - 1);
        assert!(matches!(
            volume.create_file("one more"),
            Err(Error::RootDirectoryFull)
        ));
        volume.unmount().unwrap();
        out.push(("a full fixed root directory".to_string(), path));
    }
    out
}

#[test]
fn full_volumes_and_full_fixed_roots_are_clean() {
    let mut items = Vec::new();
    for kind in [FatKind::Fat12, FatKind::Fat16, FatKind::Fat32] {
        items.extend(
            fill_scenarios(kind)
                .into_iter()
                .map(|(what, path)| (format!("{} {what}", kind_name(kind)), path)),
        );
    }
    check_all("fill", &items);
}

// ------------------------------------------------ kernel-made, then changed

#[test]
fn volumes_the_kernel_filled_are_read_back_and_then_changed_by_the_driver() {
    let dir = work_dir(WORK);
    let mut items = Vec::new();
    let mut before = Vec::new();
    for name in ["fat12", "fat16", "fat32"] {
        let source = common::image(name);
        let copy = dir.join(format!("kernel-{name}-original.img"));
        std::fs::copy(&source, &copy).unwrap();
        before.push((format!("{name} as the kernel left it"), copy.clone()));
        let changed = dir.join(format!("kernel-{name}-changed.img"));
        std::fs::copy(&source, &changed).unwrap();
        let mut volume = mount(&changed, 0);
        // Every kernel-written directory is walked by the driver first.
        let listing = view(&copy, 0);
        assert!(listing.len() > 20, "{name}: {} entries", listing.len());
        // Then changed: new files beside kernel-made ones, a rename of a
        // kernel-made long name, a delete, growth of the (possibly full) root.
        let docs = listing
            .keys()
            .find(|p| p.starts_with("docs/") && !p[5..].contains('/'))
            .cloned();
        let docs = docs.expect("the kernel made a docs directory");
        let victim = listing
            .iter()
            .find(|(p, (i, _))| !i.is_dir && p.starts_with("docs/") && !p.contains("nested"))
            .map(|(p, _)| p.clone())
            .expect("a file in docs");
        let f = volume
            .create_file("docs/written by the driver.txt")
            .unwrap();
        volume.write(&f, 0, &pattern(9000, 21)).unwrap();
        volume
            .rename(&victim, "docs/renamed by the driver with a long name.txt")
            .unwrap();
        volume.unlink("onebyte.bin").unwrap();
        let big = volume.open("exact.bin").unwrap();
        volume.truncate(&big, 100).unwrap();
        volume.write(&big, 100, &pattern(4000, 6)).unwrap();
        for index in 0..8 {
            let f = volume
                .create_file(&format!("driver root file {index}.txt"))
                .unwrap();
            volume.write(&f, 0, &pattern(700, index as u8)).unwrap();
        }
        let _ = docs;
        volume.unmount().unwrap();
        items.push((format!("{name} changed by the driver"), changed));
    }
    // The kernel's view of what it made must match the driver's reading.
    check_all("kernel-made", &before);
    check_all("kernel-made-then-changed", &items);
}

// --------------------------------------------------------- damage recipes

type Recipe = Box<dyn Fn(&Path)>;

fn patch(path: &Path, offset: usize, bytes: &[u8]) {
    let mut data = std::fs::read(path).unwrap();
    data[offset..offset + bytes.len()].copy_from_slice(bytes);
    std::fs::write(path, data).unwrap();
}

#[test]
fn the_oracles_do_reject_damage_to_a_volume_the_driver_wrote() {
    // A control: each recipe damages a copy of a good volume in one way the
    // driver could plausibly get wrong, and fsck.fat must then say so. If it
    // did not, a clean verdict above would mean nothing.
    let dir = work_dir(WORK);
    let (opts, _) = options(FatKind::Fat16);
    let good = dir.join("damage-good.img");
    let _ = std::fs::remove_file(&good);
    format_path(&good, &opts).unwrap();
    let mut volume = mount(&good, 0);
    volume.mkdir("d").unwrap();
    let a = volume
        .create_file("d/first file with a long name.bin")
        .unwrap();
    volume.write(&a, 0, &pattern(5000, 1)).unwrap();
    let b = volume.create_file("second.bin").unwrap();
    volume.write(&b, 0, &pattern(5000, 2)).unwrap();
    volume.unmount().unwrap();
    let image = std::fs::read(&good).unwrap();
    let word = |at: usize| u16::from_le_bytes([image[at], image[at + 1]]) as usize;
    let (bps, spc, reserved, fats, root_entries, spf) = (
        word(11),
        image[13] as usize,
        word(14),
        image[16] as usize,
        word(17),
        word(22),
    );
    let fat0 = reserved * bps;
    let fat1 = fat0 + spf * bps;
    let root = (reserved + fats * spf) * bps;
    let data = root + root_entries * 32;
    let cluster = spc * bps;
    let _ = (fat1, cluster, data);
    // Clusters of second.bin: find its first cluster from the root entry.
    let entry = (0..root_entries)
        .map(|i| root + i * 32)
        .find(|at| &image[*at..*at + 6] == b"SECOND")
        .expect("second.bin entry");
    let second_first = word(entry + 26);
    let recipes: Vec<(&str, Recipe)> = vec![
        (
            "FAT copies differ",
            Box::new(move |p| patch(p, fat1 + 2 * 40, &[0x55, 0x00])),
        ),
        (
            "a cluster allocated to nothing",
            Box::new(move |p| patch(p, fat0 + 2 * 3000, &[0xFF, 0xFF])),
        ),
        (
            "a file's chain cut short",
            Box::new(move |p| patch(p, fat0 + 2 * second_first, &[0xFF, 0xFF])),
        ),
        (
            "a file's size larger than its chain",
            Box::new(move |p| patch(p, entry + 28, &90_000u32.to_le_bytes())),
        ),
        (
            "a chain that loops back on itself",
            Box::new(move |p| {
                patch(
                    p,
                    fat0 + 2 * second_first,
                    &(second_first as u16).to_le_bytes(),
                )
            }),
        ),
        ("the dirty bit left set", Box::new(|p| patch(p, 37, &[1]))),
    ];
    let mut items = vec![("good".to_string(), good.clone())];
    for (index, (what, damage)) in recipes.iter().enumerate() {
        let path = dir.join(format!("damage-{index}.img"));
        std::fs::copy(&good, &path).unwrap();
        damage(&path);
        items.push((what.to_string(), path));
    }
    let images: Vec<PathBuf> = items.iter().map(|(_, p)| p.clone()).collect();
    let (inspects, tools) = inspect_images(WORK, &images);
    assert_eq!(
        inspects[0].fsck_exit, 0,
        "the control volume is clean:\n{}\n{tools}",
        inspects[0].fsck_output
    );
    for ((what, _), inspect) in items.iter().zip(&inspects).skip(1) {
        assert_ne!(
            inspect.fsck_exit, 0,
            "fsck.fat -n accepted a volume with damage ({what}); the oracle is blind to it:\n{}",
            inspect.fsck_output
        );
    }
}

#[test]
fn the_kernel_writes_into_driver_volumes_and_the_driver_keeps_writing() {
    let mut items = Vec::new();
    let mut after_driver = Vec::new();
    for kind in [FatKind::Fat12, FatKind::Fat16, FatKind::Fat32] {
        let snapshots = run_scenario("touch", kind);
        let (_, last) = snapshots.last().unwrap();
        let touched = work_dir(WORK).join(format!("{}-kernel-touched.img", kind_name(kind)));
        std::fs::copy(last, &touched).unwrap();
        oracle::exec_ok(&format!(
            "bash validation/kernel-touch.sh '{}'",
            oracle::tool_path(WORK, &touched)
        ));
        items.push((
            format!("{} after the kernel wrote", kind_name(kind)),
            touched.clone(),
        ));
        // The driver changes what the kernel left: its names, its directory.
        let again = work_dir(WORK).join(format!("{}-kernel-then-driver.img", kind_name(kind)));
        std::fs::copy(&touched, &again).unwrap();
        let mut volume = mount(&again, 0);
        let listing = volume.read_dir("").unwrap();
        assert!(
            listing
                .iter()
                .any(|e| e.name == "Kernel made file with a long name.txt"),
            "{}: the driver sees the kernel's long name",
            kind_name(kind)
        );
        assert!(listing
            .iter()
            .any(|e| e.name == "Ünïcödé kernel — résumé.txt"));
        let nested = volume.read_dir("Kernel directory").unwrap();
        assert_eq!(nested.len(), 1);
        let multi = volume.open("multi.bin").unwrap();
        let mut tail = [0u8; 23];
        let length = volume.len(&multi).unwrap();
        volume.read(&multi, length - 23, &mut tail).unwrap();
        assert_eq!(&tail, b"appended by the kernel\n");
        volume
            .rename(
                "Kernel made file with a long name.txt",
                "Kernel directory/moved by the driver.txt",
            )
            .unwrap();
        let f = volume.create_file("written after the kernel.bin").unwrap();
        volume.write(&f, 0, &pattern(30_000, 5)).unwrap();
        volume.truncate(&multi, 1234).unwrap();
        volume.unlink("KERNEL.TXT").unwrap();
        volume.unmount().unwrap();
        after_driver.push((
            format!("{} after the driver wrote over the kernel", kind_name(kind)),
            again,
        ));
    }
    check_all("kernel-touched", &items);
    check_all("kernel-then-driver", &after_driver);
}
