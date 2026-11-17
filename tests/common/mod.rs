//! What the oracle-backed tests share: the images `validation/generate.sh`
//! builds with mkfs.fat, the kernel's vfat driver and mtools, and the records
//! the independent tools left beside them. Each test binary that includes
//! this builds the images itself (in the Linux test VM, or natively on a
//! Linux CI runner), and FAILS, never skips, when it cannot.
#![cfg(feature = "oracle")]
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

// ---------------------------------------------------------------- fixtures

pub fn fixtures() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        if let Ok(dir) = std::env::var("FAT_VALIDATION_DIR") {
            return PathBuf::from(dir);
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let in_guest = std::env::var("FLTH_GUEST").as_deref() == Ok("1");
        let native = in_guest || std::env::var("FAT_ORACLE_NATIVE").as_deref() == Ok("1");
        let (out, status) = if native {
            let out = if in_guest {
                PathBuf::from("/share/fat-validation")
            } else {
                root.join("target/fat-validation")
            };
            let script = root.join("validation/generate.sh");
            let mut command = if is_root() {
                let mut c = Command::new("bash");
                c.arg(&script);
                c
            } else {
                let mut c = Command::new("sudo");
                c.args(["-n", "bash"]).arg(&script);
                c
            };
            (out.clone(), command.arg(&out).status())
        } else {
            let vm = std::env::var("FLTH_VM")
                .map(PathBuf::from)
                .unwrap_or_else(|_| root.join("../fs-linux-test-harness/scripts/vm.sh"));
            let status = Command::new(&vm)
                .current_dir(&root)
                .args([
                    "exec",
                    "bash /repo/validation/generate.sh /share/fat-validation",
                ])
                .status();
            (root.join(".vm-share/fat-validation"), status)
        };
        match status {
            Ok(status) if status.success() => out,
            other => panic!(
                "cannot build the independent-oracle images ({other:?}). They need mkfs.fat, \
                 fsck.fat, mtools and the kernel's vfat driver, which `chore test:oracle` \
                 provides in the Linux test VM (task vm:up, then this test through \
                 `vm.sh session`). On Linux CI set FAT_ORACLE_NATIVE=1 with root and the \
                 packages dosfstools and mtools."
            ),
        }
    })
}

pub fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| o.stdout == b"0\n")
        .unwrap_or(false)
}

pub fn image(name: &str) -> PathBuf {
    fixtures().join(format!("{name}.img"))
}

pub fn text(name: &str, extension: &str) -> String {
    let path = fixtures().join(format!("{name}.{extension}"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

pub fn fsck_exit(name: &str) -> i32 {
    text(name, "fsck")
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("exit="))
        .and_then(|n| n.parse().ok())
        .expect("fsck record starts with exit=N")
}

/// `<TAB>`-separated lines of a manifest or recipe, grouped by first field.
pub fn records(name: &str, extension: &str) -> BTreeMap<String, Vec<Vec<String>>> {
    let mut map: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
    for line in text(name, extension).lines() {
        let mut fields = line.split('\t').map(str::to_string);
        let key = fields.next().unwrap();
        map.entry(key).or_default().push(fields.collect());
    }
    map
}

/// Bytes of the whole image, to prove a read left it untouched.
pub fn snapshot(name: &str) -> Vec<u8> {
    std::fs::read(image(name)).unwrap()
}

pub fn assert_unwritten(name: &str, before: &[u8]) {
    assert!(
        before == snapshot(name).as_slice(),
        "{name}: the image changed while it was only read"
    );
}

/// What `command` prints where the oracles are: in the Linux test VM, or
/// directly on a Linux CI runner (`FAT_ORACLE_NATIVE=1`, or inside the guest).
pub fn oracle_output(command: &str) -> String {
    let native = std::env::var("FLTH_GUEST").as_deref() == Ok("1")
        || std::env::var("FAT_ORACLE_NATIVE").as_deref() == Ok("1");
    let output = if native {
        Command::new("bash").args(["-c", command]).output()
    } else {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let vm = std::env::var("FLTH_VM")
            .map(PathBuf::from)
            .unwrap_or_else(|_| root.join("../fs-linux-test-harness/scripts/vm.sh"));
        Command::new(vm)
            .current_dir(&root)
            .args(["exec", command])
            .output()
    };
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).into(),
        other => panic!("cannot run `{command}` where the oracles are ({other:?})"),
    }
}
