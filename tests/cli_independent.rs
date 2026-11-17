//! `fs.fat` as a process, against the images the independent tools made.
//!
//! The same images and records as `tests/independent_validation.rs`
//! (`validation/generate.sh`: mkfs.fat, the kernel's vfat driver, mtools'
//! mcopy and mshowfat, fsck.fat), but the thing under test is the
//! command-line tool, driven as a user would, and every image is checked
//! afterwards to be byte-for-byte what it was. Needs the oracles, so it is
//! behind `oracle` and FAILS, never skips, when they cannot run:
//! `chore test:oracle` provides them in the Linux test VM.
#![cfg(all(feature = "cli", feature = "oracle"))]

mod common;
#[path = "common/process.rs"]
mod process;

use common::{assert_unwritten, fixtures, fsck_exit, image, oracle_output, records, snapshot};
use process::{fs, lines, ok, run};
use std::collections::BTreeMap;

const KINDS: [(&str, &str); 3] = [("fat12", "FAT12"), ("fat16", "FAT16"), ("fat32", "FAT32")];

fn path(name: &str) -> String {
    image(name).to_str().unwrap().to_string()
}

fn field(text: &str, key: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}: ")))
        .unwrap_or_else(|| panic!("no `{key}` in:\n{text}"))
        .to_string()
}

/// What the independent readers say the volume holds: path -> (directory, size).
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

fn listed(name: &str, extra: &[&str]) -> BTreeMap<String, (bool, u64)> {
    let mut args = vec!["ls", "-R", "/", "--text"];
    args.extend_from_slice(extra);
    let image = path(name);
    lines(&ok(fs(std::path::Path::new(&image), &args)))
        .into_iter()
        .map(|e| {
            (
                e["path"].trim_start_matches('/').to_string(),
                (e["type"] == "directory", e["size"].parse().unwrap()),
            )
        })
        .collect()
}

fn run_on(name: &str, args: &[&str]) -> std::process::Output {
    let image = path(name);
    fs(std::path::Path::new(&image), args)
}

fn first_cluster(name: &str, file: &str) -> String {
    let text = ok(run_on(name, &["stat", &format!("/{file}"), "--text"]));
    field(&text, "first_cluster").trim().to_string()
}

#[test]
fn clean_images_list_read_and_chain_like_the_kernel_and_mtools() {
    for (name, variant) in KINDS {
        let before = snapshot(name);
        assert_eq!(fsck_exit(name), 0, "{name}: fsck.fat calls the image clean");
        let info = ok(run_on(name, &["info", "--text"]));
        assert_eq!(field(&info, "valid"), "true", "{name}");
        assert_eq!(field(&info, "volume.variant"), variant, "{name}");

        let want = expected(name);
        assert_eq!(
            listed(name, &[]),
            want,
            "{name}: ls -R differs from the kernel/mtools view"
        );

        let tree = fixtures().join(format!("{name}.tree"));
        for (file, (directory, _)) in &want {
            if *directory {
                continue;
            }
            let out = run_on(name, &["cat", &format!("/{file}")]);
            assert!(out.status.success(), "{name}: cat {file}");
            assert!(
                out.stdout == std::fs::read(tree.join(file)).unwrap(),
                "{name}: {file}: bytes differ from the kernel's copy"
            );
        }

        let mut fragmented = 0;
        for fields in &records(name, "manifest")["chain"] {
            let theirs = fields[1].trim();
            if theirs.is_empty() {
                continue;
            }
            let first = first_cluster(name, &fields[0]);
            let text = ok(run_on(
                name,
                &["chain", &first, "--limit", "100000", "--text"],
            ));
            let ours = field(&text, "clusters");
            assert_eq!(
                ours, theirs,
                "{name}: {}: chain differs from mshowfat",
                fields[0]
            );
            assert_eq!(
                field(&text, "stopped"),
                "end_of_chain",
                "{name}: {}",
                fields[0]
            );
            let clusters: Vec<u32> = theirs.split(',').map(|c| c.parse().unwrap()).collect();
            if clusters.windows(2).any(|w| w[1] != w[0] + 1) {
                fragmented += 1;
            }
        }
        assert!(
            fragmented >= 2,
            "{name}: the fixture holds fragmented files"
        );
        assert_unwritten(name, &before);
    }
}

#[test]
fn export_verify_gives_the_kernels_bytes() {
    for (name, _) in KINDS {
        let out = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("{name}-export.bin"));
        let _ = std::fs::remove_file(&out);
        let report = ok(run_on(
            name,
            &[
                "export",
                "/docs/nested/deep.bin",
                out.to_str().unwrap(),
                "--verify",
            ],
        ));
        assert!(report.contains("\"verified\": true"), "{name}: {report}");
        let tree = fixtures().join(format!("{name}.tree/docs/nested/deep.bin"));
        assert!(
            std::fs::read(&out).unwrap() == std::fs::read(tree).unwrap(),
            "{name}"
        );
    }
}

#[test]
fn deleted_files_leave_the_listing_and_stay_in_the_forensic_scan() {
    for (base, _) in KINDS {
        let name = format!("{base}-deleted");
        let before = snapshot(&name);
        assert_eq!(listed(&name, &[]), expected(&name), "{name}: live listing");

        let mut want: Vec<String> = records(&name, "recipe")["deleted"]
            .iter()
            .map(|f| f[0].clone())
            .collect();
        want.sort();
        let found = lines(&ok(run_on(
            &name,
            &["scan", "--deleted", "-R", "/", "--text"],
        )));
        let mut sizes: Vec<String> = found
            .iter()
            .filter(|r| r["raw"].starts_with("e5"))
            .map(|r| r["size"].clone())
            .collect();
        sizes.sort();
        assert_eq!(sizes, want, "{name}: deleted records");
        assert!(found.iter().all(|r| r["deleted"] == "true"), "{name}");
        // The nested one is reported with its directory.
        assert!(
            found.iter().any(|r| r["directory"] == "/docs/nested"),
            "{name}"
        );
        assert_unwritten(&name, &before);
    }
}

#[test]
fn a_truncated_chain_fails_that_file_only() {
    for (base, _) in KINDS {
        let name = format!("{base}-truncated");
        let before = snapshot(&name);
        assert_ne!(fsck_exit(&name), 0, "{name}: fsck.fat sees the damage");
        let tree = fixtures().join(format!("{base}.tree"));
        for (file, (directory, _)) in expected(base) {
            if directory {
                continue;
            }
            let out = run_on(&name, &["cat", &format!("/{file}")]);
            if file == "docs/nested/deep.bin" {
                assert_eq!(out.status.code(), Some(1), "{name}");
                assert!(out.stdout.is_empty(), "{name}");
                assert!(
                    String::from_utf8_lossy(&out.stderr).contains("chain ends before file size"),
                    "{name}"
                );
                let first = first_cluster(&name, &file);
                let chain = ok(run_on(&name, &["chain", &first, "--text"]));
                assert_eq!(field(&chain, "count"), "1", "{name}");
                assert_eq!(field(&chain, "stopped"), "end_of_chain", "{name}");
            } else {
                assert!(out.status.success(), "{name}: {file}");
                assert!(
                    out.stdout == std::fs::read(tree.join(&file)).unwrap(),
                    "{name}: {file}"
                );
            }
        }
        assert_unwritten(&name, &before);
    }
}

#[test]
fn damaged_boot_sectors_are_reported_and_a_fat32_backup_reads_the_volume() {
    for (base, _) in KINDS {
        for (recipe, oracle_rejects) in [("nobootsig", false), ("zeroboot", true), ("badbpb", true)]
        {
            let name = format!("{base}-{recipe}");
            let before = snapshot(&name);
            assert_eq!(
                fsck_exit(&name) != 0,
                oracle_rejects,
                "{name}: fsck.fat verdict"
            );

            let info = ok(run_on(&name, &["info", "--text"]));
            assert_eq!(field(&info, "valid"), "false", "{name}");
            assert!(!field(&info, "problem").is_empty(), "{name}");

            let refused = run_on(&name, &["ls", "/"]);
            assert_eq!(refused.status.code(), Some(4), "{name}");
            assert!(refused.stdout.is_empty(), "{name}");

            if base == "fat32" {
                assert_eq!(field(&info, "backup.accepted"), "true", "{name}");
                assert_eq!(field(&info, "backup.suggest"), "--boot-sector 6", "{name}");
                assert_eq!(
                    listed_with_boot(&name),
                    expected(base),
                    "{name}: backup listing"
                );
                let tree = fixtures().join(format!("{base}.tree"));
                let out = run_on(
                    &name,
                    &["--boot-sector", "6", "cat", "/docs/nested/deep.bin"],
                );
                assert!(
                    out.stdout == std::fs::read(tree.join("docs/nested/deep.bin")).unwrap(),
                    "{name}"
                );
            } else {
                // FAT12 and FAT16 keep no backup boot sector.
                assert!(!info.contains("backup.suggest"), "{name}: {info}");
            }
            assert_unwritten(&name, &before);
        }
    }
}

fn listed_with_boot(name: &str) -> BTreeMap<String, (bool, u64)> {
    let image = path(name);
    lines(&ok(fs(
        std::path::Path::new(&image),
        &["--boot-sector", "6", "ls", "-R", "/", "--text"],
    )))
    .into_iter()
    .map(|e| {
        (
            e["path"].trim_start_matches('/').to_string(),
            (e["type"] == "directory", e["size"].parse().unwrap()),
        )
    })
    .collect()
}

#[test]
fn the_dotted_name_collides_with_nothing_dosfstools_or_mtools_install() {
    let ours = ok(run(&["generate", "names"]));
    let files = oracle_output("dpkg -L dosfstools mtools");
    let installed: Vec<&str> = files.lines().filter_map(|l| l.rsplit('/').next()).collect();
    for name in ours.split_whitespace().chain(["rust-fs-fat"]) {
        assert!(
            !installed.contains(&name),
            "{name} is installed by dosfstools or mtools"
        );
    }
    // The names this tool is deliberately not using are theirs.
    for theirs in ["mkfs.fat", "fsck.fat"] {
        assert!(
            installed.contains(&theirs),
            "{theirs} comes from dosfstools"
        );
    }
}
