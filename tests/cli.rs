//! `fs.fat` as a process, on the two small volumes in `tests/cli/`.
//!
//! `sample-fat12.img.gz` and `sample-fat32.img.gz` were formatted by
//! `mkfs.fat` and filled by mtools (`validation/make-cli-samples.sh`), and
//! their `.manifest` files hold what mtools' `mshowfat` said of the chains, so
//! these checks compare the tool with something that is not the driver, and
//! need no VM. The same tool against every image the kernel, mkfs.fat and
//! mtools produce is `tests/cli_independent.rs`.
#![cfg(feature = "cli")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "common/process.rs"]
mod process;

use process::{fs, lines, ok, run};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cli")
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A fresh decompressed copy of a sample, checked against the digest of the
/// volume the committed manifest describes.
fn sample(name: &str, dir: &Path) -> PathBuf {
    let compressed = root().join(format!("sample-{name}.img.gz"));
    let out = Command::new("gzip")
        .arg("-dc")
        .arg(&compressed)
        .output()
        .unwrap_or_else(|e| panic!("gzip is needed to unpack {}: {e}", compressed.display()));
    assert!(out.status.success(), "gzip -dc {}", compressed.display());
    let digests = std::fs::read_to_string(root().join("samples.sha256")).unwrap();
    let want = digests
        .lines()
        .find(|l| l.ends_with(&format!("sample-{name}.img")))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    assert_eq!(
        sha256(&out.stdout),
        want,
        "sample-{name}.img.gz is not the volume samples.sha256 names"
    );
    let path = dir.join(format!("{name}.img"));
    std::fs::write(&path, &out.stdout).unwrap();
    path
}

fn sha256(bytes: &[u8]) -> String {
    use std::io::Write;
    let mut child = Command::new(if cfg!(target_os = "macos") {
        "shasum"
    } else {
        "sha256sum"
    })
    .args(if cfg!(target_os = "macos") {
        vec!["-a", "256"]
    } else {
        vec![]
    })
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .spawn()
    .expect("sha256sum or shasum");
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

struct Manifest {
    files: Vec<(String, u64)>,
    dirs: Vec<String>,
    chains: BTreeMap<String, Vec<u32>>,
    deleted: (String, u64, Vec<u32>),
}

fn manifest(name: &str) -> Manifest {
    let text = std::fs::read_to_string(root().join(format!("sample-{name}.manifest"))).unwrap();
    let mut m = Manifest {
        files: vec![],
        dirs: vec![],
        chains: BTreeMap::new(),
        deleted: (String::new(), 0, vec![]),
    };
    let clusters = |s: &str| {
        s.split(',')
            .map(|c| c.parse().unwrap())
            .collect::<Vec<u32>>()
    };
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "d" => m.dirs.push(f[2].to_string()),
            "f" => m.files.push((f[2].to_string(), f[1].parse().unwrap())),
            "chain" => {
                m.chains.insert(f[1].to_string(), clusters(f[2]));
            }
            "deleted" => m.deleted = (f[1].to_string(), f[2].parse().unwrap(), clusters(f[4])),
            other => panic!("manifest line {other}"),
        }
    }
    m
}

/// What the generator wrote into a file, to compare `cat` with.
fn content(path: &str) -> Vec<u8> {
    fn lines(prefix: &str, count: usize, bytes: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 1..=count {
            out.extend(format!("{prefix} line {i:04}\n").bytes());
        }
        out.truncate(bytes);
        out
    }
    match path {
        "README.TXT" => b"sample fat volume\n".to_vec(),
        "empty.dat" => vec![],
        "docs/Guide With Spaces.txt" => lines("guide", 200, 1300),
        "docs/deep/leaf.bin" => lines("leaf", 400, 2000),
        other => panic!("no generator for {other}"),
    }
}

const SAMPLES: [(&str, &str); 2] = [("fat12", "FAT12"), ("fat32", "FAT32")];

#[test]
fn the_tool_is_named_fs_fat_and_answers_for_itself() {
    assert_eq!(ok(run(&["generate", "names"])).trim(), "fs.fat");
    assert_eq!(
        ok(run(&["--version"])).trim(),
        format!("rust-fs-fat (rust-fs-fat32) {}", env!("CARGO_PKG_VERSION"))
    );
    let help = ok(run(&["fs", "--help"]));
    assert!(help.contains("read-only"), "{help}");
    for verb in [
        "info", "ls", "stat", "cat", "export", "scan", "chain", "fat",
    ] {
        assert!(help.contains(&format!("\n  {verb} ")), "help lists {verb}");
    }
}

#[test]
fn info_reports_the_boot_sector_and_variant_of_each_sample() {
    let dir = scratch("info");
    for (name, variant) in SAMPLES {
        let image = sample(name, &dir);
        let text = ok(fs(&image, &["info", "--text"]));
        assert!(text.contains("valid: true"), "{name}: {text}");
        assert!(
            text.contains(&format!("volume.variant: {variant}")),
            "{name}: {text}"
        );
        assert!(text.contains("boot_sector.oem: mkfs.fat"), "{name}: {text}");
        assert!(
            text.contains(&format!(
                "boot_sector.volume_label: SAMPLE{}",
                &variant[3..]
            )),
            "{text}"
        );
    }
}

#[test]
fn listing_matches_what_mtools_put_on_the_volume() {
    let dir = scratch("ls");
    for (name, _) in SAMPLES {
        let image = sample(name, &dir);
        let want = manifest(name);
        let all = lines(&ok(fs(&image, &["ls", "-R", "/", "--text"])));
        let mut got: Vec<(String, String, u64)> = all
            .iter()
            .map(|e| {
                (
                    e["path"].trim_start_matches('/').to_string(),
                    e["type"].clone(),
                    e["size"].parse().unwrap(),
                )
            })
            .collect();
        got.sort();
        let mut expected: Vec<(String, String, u64)> = want
            .dirs
            .iter()
            .map(|d| (d.clone(), "directory".into(), 0))
            .chain(
                want.files
                    .iter()
                    .map(|(p, s)| (p.clone(), "file".into(), *s)),
            )
            .collect();
        expected.sort();
        assert_eq!(got, expected, "{name}");
        // The deleted file is not listed.
        assert!(!all.iter().any(|e| e["name"] == "gone.txt"), "{name}");
        // A subdirectory lists, names match without regard to case.
        let docs = lines(&ok(fs(&image, &["ls", "/DOCS", "--text"])));
        let mut names: Vec<&str> = docs.iter().map(|e| e["name"].as_str()).collect();
        names.sort();
        assert_eq!(names, ["Guide With Spaces.txt", "deep"], "{name}");
    }
}

#[test]
fn cat_and_export_give_the_bytes_the_files_were_made_from() {
    let dir = scratch("cat");
    for (name, _) in SAMPLES {
        let image = sample(name, &dir);
        for (path, size) in manifest(name).files {
            let out = fs(&image, &["cat", &format!("/{path}")]);
            assert!(out.status.success(), "{name}: cat {path}");
            assert_eq!(out.stdout, content(&path), "{name}: {path}");
            assert_eq!(out.stdout.len() as u64, size);
        }
        let exported = dir.join(format!("{name}.exported"));
        let report = ok(fs(
            &image,
            &[
                "export",
                "/docs/deep/leaf.bin",
                exported.to_str().unwrap(),
                "--verify",
            ],
        ));
        assert!(report.contains("\"verified\": true"), "{report}");
        assert_eq!(
            std::fs::read(&exported).unwrap(),
            content("docs/deep/leaf.bin")
        );
    }
}

#[test]
fn chains_match_mshowfat() {
    let dir = scratch("chain");
    for (name, _) in SAMPLES {
        let image = sample(name, &dir);
        let want = manifest(name);
        for (path, clusters) in &want.chains {
            let first = clusters[0].to_string();
            let text = ok(fs(&image, &["chain", &first, "--text"]));
            assert!(
                text.contains(&format!(
                    "clusters: {}",
                    clusters
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                )),
                "{name}: {path}: {text}"
            );
            assert!(
                text.contains("stopped: end_of_chain"),
                "{name}: {path}: {text}"
            );
        }
        let one = ok(fs(
            &image,
            &["fat", &want.chains["README.TXT"][0].to_string(), "--text"],
        ));
        assert!(one.contains("value=end_of_chain"), "{one}");
    }
}

#[test]
fn scan_finds_the_deleted_record_and_its_cleared_chain_reads_as_free() {
    let dir = scratch("scan");
    for (name, _) in SAMPLES {
        let image = sample(name, &dir);
        let want = manifest(name);
        let found = lines(&ok(fs(&image, &["scan", "--deleted", "--text"])));
        assert_eq!(found.len(), 1, "{name}: {found:?}");
        let record = &found[0];
        assert_eq!(record["deleted"], "true");
        assert_eq!(record["size"], want.deleted.1.to_string());
        // The first character is lost to the deletion mark; the NT lower-case
        // flags the file was written with still apply to the rest.
        assert_eq!(record["name"], "_one.txt", "{name}: {record:?}");
        assert!(record["raw"].starts_with("e5"), "{name}: {record:?}");
        // The first cluster the record still names: the FAT holds nothing there.
        assert_eq!(record["first_cluster"], want.deleted.2[0].to_string());
        let chain = ok(fs(&image, &["chain", &record["first_cluster"], "--text"]));
        assert!(chain.contains("stopped: free"), "{name}: {chain}");
        // Without --deleted the live records are there too.
        let all = lines(&ok(fs(&image, &["scan", "-R", "/", "--text"])));
        assert!(all.len() > found.len(), "{name}");
    }
}

#[test]
fn a_damaged_boot_sector_is_inspected_and_the_backup_reads_the_volume() {
    let dir = scratch("damaged");
    let image = sample("fat32", &dir);
    let mut bytes = std::fs::read(&image).unwrap();
    bytes[..512].fill(0);
    std::fs::write(&image, &bytes).unwrap();

    let info = ok(fs(&image, &["info", "--text"]));
    assert!(info.contains("valid: false"), "{info}");
    assert!(info.contains("missing boot-sector signature"), "{info}");
    assert!(info.contains("backup.accepted: true"), "{info}");
    assert!(info.contains("backup.suggest: --boot-sector 6"), "{info}");

    let refused = fs(&image, &["ls", "/"]);
    assert_eq!(refused.status.code(), Some(4));
    assert!(refused.stdout.is_empty());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--boot-sector"));

    let listed = lines(&ok(fs(
        &image,
        &["--boot-sector", "6", "ls", "/", "--text"],
    )));
    assert!(listed.iter().any(|e| e["name"] == "README.TXT"));
    let cat = fs(&image, &["--boot-sector", "6", "cat", "/README.TXT"]);
    assert_eq!(cat.stdout, content("README.TXT"));
    assert_eq!(
        std::fs::read(&image).unwrap(),
        bytes,
        "the image was written"
    );
}

#[test]
fn errors_use_the_shared_contract() {
    let dir = scratch("errors");
    let image = sample("fat12", &dir);
    let missing = fs(&image, &["cat", "/absent"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    let error = String::from_utf8_lossy(&missing.stderr);
    assert!(
        error.contains("\"kind\": \"not_found\"") || error.contains("not_found"),
        "{error}"
    );
    assert_eq!(fs(&image, &["cat", "/docs"]).status.code(), Some(1));
    assert_eq!(fs(&image, &["ls", "/README.TXT/x"]).status.code(), Some(1));
    assert_eq!(fs(&image, &["ls", "/.."]).status.code(), Some(2));
    assert_eq!(
        fs(&image, &["--offset", "100", "info"]).status.code(),
        Some(2)
    );
    assert_eq!(fs(&image, &["frobnicate"]).status.code(), Some(2));
    assert_eq!(
        run(&["fs", "/no/such/image", "info"]).status.code(),
        Some(1)
    );
    // A bound stops a recursive walk with the shared error.
    let limited = fs(&image, &["ls", "-R", "--limit", "2", "/"]);
    assert_eq!(limited.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&limited.stderr).contains("--limit 2 reached"));
    let shallow = fs(&image, &["ls", "-R", "--max-depth", "1", "/"]);
    assert_eq!(shallow.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&shallow.stderr).contains("--max-depth 1 reached"));
}

#[test]
fn nothing_writes_the_image_not_even_a_read_only_one() {
    let dir = scratch("readonly");
    for (name, _) in SAMPLES {
        let image = sample(name, &dir);
        let before = std::fs::read(&image).unwrap();
        let mut permissions = std::fs::metadata(&image).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&image, permissions).unwrap();
        let export = dir.join(format!("{name}.out"));
        for args in [
            vec!["info"],
            vec!["ls", "-R", "/"],
            vec!["stat", "/docs"],
            vec!["cat", "/README.TXT"],
            vec!["scan", "-R", "/"],
            vec!["chain", "4"],
            vec!["fat", "2", "--count", "8"],
            vec!["export", "/README.TXT", export.to_str().unwrap()],
        ] {
            let out = fs(&image, &args);
            assert!(
                out.status.success(),
                "{name}: {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert_eq!(
            std::fs::read(&image).unwrap(),
            before,
            "{name}: image changed"
        );
    }
}
