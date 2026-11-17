//! Running the independent tools from a test.
//!
//! The tools (dosfstools, mtools, the kernel's vfat driver) live in the Linux
//! test VM, or natively on a Linux CI runner. A test writes its images into
//! [`work_dir`] and asks [`exec`] to run something where the tools are. A test
//! that cannot reach them FAILS and names the task that provides them; it does
//! not skip.
#![allow(dead_code)]

pub mod scenario;
pub mod sha256;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Inside the VM (`FLTH_GUEST=1`): run directly.
    Guest,
    /// On a Linux host with the tools and root (`FAT_ORACLE_NATIVE=1`).
    Native,
    /// On the host, with the tools in the VM (`chore test:oracle`).
    Vm,
}

pub fn mode() -> Mode {
    if std::env::var("FLTH_GUEST").as_deref() == Ok("1") {
        Mode::Guest
    } else if std::env::var("FAT_ORACLE_NATIVE").as_deref() == Ok("1") {
        Mode::Native
    } else {
        Mode::Vm
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Where a test writes the files the tools read, as the test sees it.
pub fn work_dir(name: &str) -> PathBuf {
    let dir = match mode() {
        Mode::Guest => PathBuf::from("/share").join(name),
        Mode::Native => root().join("target").join(name),
        Mode::Vm => root().join(".vm-share").join(name),
    };
    std::fs::create_dir_all(&dir).expect("create the oracle work directory");
    dir
}

/// The path of a file under [`work_dir`] as the tools see it.
pub fn tool_path(name: &str, file: &Path) -> String {
    let relative = file
        .strip_prefix(work_dir(name))
        .expect("the file is under the work directory");
    match mode() {
        Mode::Vm | Mode::Guest => format!("/share/{name}/{}", relative.display()),
        Mode::Native => file.display().to_string(),
    }
}

static SERIAL: Mutex<()> = Mutex::new(());

/// Runs a shell command where the tools are and returns its output. The
/// command's working directory is the repository root.
pub fn exec(command: &str) -> Output {
    let _turn = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let result = match mode() {
        Mode::Guest => Command::new("bash")
            .args(["-c", command])
            .current_dir("/repo")
            .output(),
        Mode::Native => {
            let mut c = if is_root() {
                Command::new("bash")
            } else {
                let mut c = Command::new("sudo");
                c.args(["-n", "bash"]);
                c
            };
            c.args(["-c", command]).current_dir(root()).output()
        }
        Mode::Vm => {
            let vm = std::env::var("FLTH_VM")
                .map(PathBuf::from)
                .unwrap_or_else(|_| root().join("../fs-linux-test-harness/scripts/vm.sh"));
            Command::new(vm)
                .current_dir(root())
                .args(["exec", &format!("cd /repo && {command}")])
                .output()
        }
    };
    match result {
        Ok(output) => output,
        Err(error) => panic!("{}", unavailable(&format!("{error}"))),
    }
}

/// [`exec`], requiring success.
pub fn exec_ok(command: &str) -> String {
    let output = exec(command);
    if !output.status.success() {
        panic!(
            "`{command}` failed ({}):\n{}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            unavailable("the command failed")
        );
    }
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn unavailable(why: &str) -> String {
    format!(
        "the independent oracles cannot run ({why}). They need dosfstools, mtools and the \
         kernel's vfat driver, which `chore test:oracle` provides in the Linux test VM \
         (task vm:up, then the tests through `vm.sh session`). On Linux CI set \
         FAT_ORACLE_NATIVE=1 with root and the packages dosfstools and mtools."
    )
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| o.stdout == b"0\n")
        .unwrap_or(false)
}

/// What `validation/check-image.sh` recorded about one image.
#[derive(Debug)]
pub struct Check {
    pub fsck_exit: i32,
    pub fsck_output: String,
    pub mounted: bool,
    pub mount_error: String,
    /// Cluster size, total clusters and free clusters the kernel reports.
    pub statfs: Option<(u64, u64, u64)>,
    pub fs_type: String,
    pub entries: Option<u64>,
    pub label: String,
}

/// Runs the independent checks over `images` (files under `work`) in one call
/// and returns each image's record, in order, with the tool versions.
pub fn check_images(work: &str, images: &[PathBuf]) -> (Vec<Check>, String) {
    let paths: Vec<String> = images.iter().map(|p| tool_path(work, p)).collect();
    exec_ok(&format!(
        "bash validation/check-image.sh {}",
        paths
            .iter()
            .map(|p| format!("'{p}'"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    let tools = std::fs::read_to_string(images[0].parent().unwrap().join("tools.txt"))
        .expect("tools.txt is written beside the images");
    let checks = images
        .iter()
        .map(|image| {
            let record = std::fs::read_to_string(format!("{}.check", image.display()))
                .unwrap_or_else(|e| panic!("no check record for {}: {e}", image.display()));
            parse_check(&record)
        })
        .collect();
    (checks, tools)
}

fn parse_check(record: &str) -> Check {
    let mut check = Check {
        fsck_exit: -1,
        fsck_output: String::new(),
        mounted: false,
        mount_error: String::new(),
        statfs: None,
        fs_type: String::new(),
        entries: None,
        label: String::new(),
    };
    for line in record.lines() {
        let (key, value) = line.split_once('\t').unwrap_or((line, ""));
        match key {
            "fsck_exit" => check.fsck_exit = value.parse().unwrap(),
            "fsck_output" => {
                check.fsck_output.push_str(value);
                check.fsck_output.push('\n');
            }
            "mount" => check.mounted = value == "ok",
            "mount_error" => check.mount_error.push_str(value),
            "statfs" => {
                let f: Vec<&str> = value.split(' ').collect();
                check.statfs = Some((
                    f[0].parse().unwrap(),
                    f[1].parse().unwrap(),
                    f[2].parse().unwrap(),
                ));
                check.fs_type = f[3].to_string();
            }
            "entries" => check.entries = value.parse().ok(),
            "label" => check.label = value.to_string(),
            _ => {}
        }
    }
    check
}

/// One entry as the kernel's vfat driver shows it.
#[derive(Clone, Debug)]
pub struct KernelEntry {
    pub dir: bool,
    pub size: u64,
    pub mtime: f64,
    pub ctime: f64,
    pub atime: i64,
    pub sha: String,
}

/// One entry as mtools copied it out.
#[derive(Clone, Debug)]
pub struct ToolsEntry {
    pub dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub sha: String,
}

/// What `validation/inspect-image.sh` recorded about one image.
#[derive(Debug, Default)]
pub struct Inspect {
    pub fsck_exit: i32,
    pub fsck_output: String,
    pub kernel_mounted: bool,
    pub mount_error: String,
    pub kernel: BTreeMap<String, KernelEntry>,
    pub mtools_exit: i32,
    pub mtools_error: String,
    pub mtools: BTreeMap<String, ToolsEntry>,
    pub mdir_exit: i32,
}

/// Runs the three independent tools over `images` (under `work`) in one call.
/// Returns each image's record and the tool versions.
pub fn inspect_images(work: &str, images: &[PathBuf]) -> (Vec<Inspect>, String) {
    let paths: Vec<String> = images.iter().map(|p| tool_path(work, p)).collect();
    exec_ok(&format!(
        "bash validation/inspect-image.sh {}",
        paths
            .iter()
            .map(|p| format!("'{p}'"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    let tools = std::fs::read_to_string(images[0].parent().unwrap().join("tools.txt"))
        .expect("tools.txt is written beside the images");
    let records = images
        .iter()
        .map(|image| {
            let bytes = std::fs::read(format!("{}.inspect", image.display()))
                .unwrap_or_else(|e| panic!("no inspect record for {}: {e}", image.display()));
            parse_inspect(&String::from_utf8_lossy(&bytes))
        })
        .collect();
    (records, tools)
}

fn parse_inspect(record: &str) -> Inspect {
    let mut inspect = Inspect::default();
    for line in record.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields[0] {
            "fsck_exit" => inspect.fsck_exit = fields[1].parse().unwrap(),
            "fsck_output" => {
                inspect
                    .fsck_output
                    .push_str(fields.get(1).copied().unwrap_or(""));
                inspect.fsck_output.push('\n');
            }
            "kernel_mount" => inspect.kernel_mounted = fields[1] == "ok",
            "mount_error" => inspect
                .mount_error
                .push_str(fields.get(1).copied().unwrap_or("")),
            "kernel" => {
                inspect.kernel.insert(
                    fields[7].to_string(),
                    KernelEntry {
                        dir: fields[1] == "d",
                        size: fields[2].parse().unwrap(),
                        mtime: fields[3].parse().unwrap(),
                        ctime: fields[4].parse().unwrap(),
                        atime: fields[5].parse().unwrap(),
                        sha: fields[6].to_string(),
                    },
                );
            }
            "mtools_exit" => inspect.mtools_exit = fields[1].parse().unwrap(),
            "mtools_error" => inspect
                .mtools_error
                .push_str(fields.get(1).copied().unwrap_or("")),
            "mtools" => {
                inspect.mtools.insert(
                    fields[5].to_string(),
                    ToolsEntry {
                        dir: fields[1] == "d",
                        size: fields[2].parse().unwrap(),
                        mtime: fields[3].parse().unwrap(),
                        sha: fields[4].to_string(),
                    },
                );
            }
            "mdir_exit" => inspect.mdir_exit = fields[1].parse().unwrap(),
            _ => {}
        }
    }
    inspect
}
