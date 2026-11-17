//! Windows' own opinion of volumes this driver wrote.
//!
//! Each volume is formatted and changed by the driver inside a partitioned
//! disk image (an MBR with one partition at 1 MiB, which also exercises the
//! volume offset), wrapped as a fixed VHD, shipped to the Windows test VM of
//! `fs-windows-test-harness` and attached. `validation/windows-check.ps1`
//! runs `chkdsk` (without /F, so nothing is repaired) and lists every entry
//! with its size, times and SHA-256; this file requires chkdsk to find no
//! problems and the listing to match the driver's own view of the volume.
//!
//! `chore test:windows` runs it. It FAILS (never skips) when the VM cannot be
//! reached. The ssh configuration is `$FSWTH_SSH_CONFIG`, by default the one
//! the harness writes to `~/.local/share/fs-windows-test-harness/arm64/`, and
//! the host is `$FSWTH_HOST` (default `fswth-local`).
#![cfg(feature = "windows-oracle")]

mod oracle;

use fs_fat32::mkfs::format_path;
use fs_fat32::timestamp::FatTimestamp;
use fs_fat32::FatKind;
use oracle::scenario::{kind_name, mount, options, scenario, view, View};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

static VM: std::sync::Mutex<()> = std::sync::Mutex::new(());

type Damage = Box<dyn Fn(&mut Vec<u8>)>;

const OFFSET: u64 = 1024 * 1024;

fn ssh_config() -> String {
    std::env::var("FSWTH_SSH_CONFIG").unwrap_or_else(|_| {
        format!(
            "{}/.local/share/fs-windows-test-harness/arm64/ssh_config",
            std::env::var("HOME").unwrap_or_default()
        )
    })
}

fn host() -> String {
    std::env::var("FSWTH_HOST").unwrap_or_else(|_| "fswth-local".to_string())
}

fn unreachable(why: &str) -> String {
    format!(
        "the Windows test VM cannot be reached ({why}). `chore test:windows` needs the guest of \
         fs-windows-test-harness: bring it up with `python3 ../fs-windows-test-harness/scripts/local-vm.py up` \
         (see its docs/qemu-vm.md); the ssh configuration is {} and the host {}",
        ssh_config(),
        host()
    )
}

fn ssh(command: &str) -> String {
    let output = Command::new("ssh")
        .args([
            "-F",
            &ssh_config(),
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=20",
            &host(),
            command,
        ])
        .output()
        .unwrap_or_else(|e| panic!("{}", unreachable(&e.to_string())));
    assert!(
        output.status.success(),
        "{}\n`{command}` failed ({}):\n{}\n{}",
        unreachable("the command failed"),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn scp(from: &str, to: &str) {
    let output = Command::new("scp")
        .args([
            "-F",
            &ssh_config(),
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=20",
            from,
            to,
        ])
        .output()
        .unwrap_or_else(|e| panic!("{}", unreachable(&e.to_string())));
    assert!(
        output.status.success(),
        "{}\nscp {from} {to} failed: {}",
        unreachable("scp failed"),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn work_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/fat-windows");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A disk image: a 1 MiB gap holding an MBR with one partition, then the volume.
fn make_disk(path: &Path, kind: FatKind) {
    let (mut opts, size) = options(kind);
    opts.offset = OFFSET;
    let _ = std::fs::remove_file(path);
    format_path(path, &opts).unwrap();
    let mut mbr = vec![0u8; 512];
    let entry = &mut mbr[446..462];
    entry[0] = 0x00;
    entry[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    entry[4] = match kind {
        FatKind::Fat12 => 0x01,
        FatKind::Fat16 => 0x06,
        FatKind::Fat32 => 0x0C,
    };
    entry[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    entry[8..12].copy_from_slice(&((OFFSET / 512) as u32).to_le_bytes());
    entry[12..16].copy_from_slice(&((size / 512) as u32).to_le_bytes());
    mbr[510..512].copy_from_slice(&[0x55, 0xAA]);
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&mbr).unwrap();
}

/// The disk as a fixed VHD: the raw image followed by the 512-byte footer.
fn write_vhd(image: &Path, vhd: &Path) {
    let mut data = std::fs::read(image).unwrap();
    let size = data.len() as u64;
    let sectors = size / 512;
    // CHS geometry as the VHD specification computes it.
    let total = sectors.min(65535 * 16 * 255);
    let (cylinders, heads, per_track) = if total >= 65535 * 16 * 63 {
        (total / (255 * 17) / 255, 255u64, 255u64)
    } else {
        let mut per_track = 17u64;
        let mut cylinders_times_heads = total / per_track;
        let mut heads = cylinders_times_heads.div_ceil(1024).max(4);
        if cylinders_times_heads >= heads * 1024 || heads > 16 {
            per_track = 31;
            heads = 16;
            cylinders_times_heads = total / per_track;
        }
        if cylinders_times_heads >= heads * 1024 {
            per_track = 63;
            heads = 16;
            cylinders_times_heads = total / per_track;
        }
        (cylinders_times_heads / heads, heads, per_track)
    };
    let mut footer = vec![0u8; 512];
    footer[0..8].copy_from_slice(b"conectix");
    footer[8..12].copy_from_slice(&2u32.to_be_bytes());
    footer[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    footer[16..24].copy_from_slice(&u64::MAX.to_be_bytes());
    footer[24..28].copy_from_slice(&0u32.to_be_bytes());
    footer[28..32].copy_from_slice(b"rfat");
    footer[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    footer[36..40].copy_from_slice(b"Wi2k");
    footer[40..48].copy_from_slice(&size.to_be_bytes());
    footer[48..56].copy_from_slice(&size.to_be_bytes());
    footer[56..58].copy_from_slice(&(cylinders as u16).to_be_bytes());
    footer[58] = heads as u8;
    footer[59] = per_track as u8;
    footer[60..64].copy_from_slice(&2u32.to_be_bytes());
    footer[68..84].copy_from_slice(&[
        0xFA, 0x7F, 0xA7, 0x01, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x01,
    ]);
    let sum: u32 = footer.iter().map(|b| u32::from(*b)).sum();
    footer[64..68].copy_from_slice(&(!sum).to_be_bytes());
    data.extend_from_slice(&footer);
    std::fs::write(vhd, data).unwrap();
}

fn local_time(stamp: FatTimestamp) -> String {
    let (seconds, _) = stamp.to_unix().expect("a real time");
    // The stored fields are local time; render them without any zone.
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

struct Record {
    chkdsk_exit: i32,
    chkdsk_output: String,
    fs: String,
    label: String,
    entries: BTreeMap<String, (bool, u64, String, String, String)>,
}

fn parse(result: &str) -> Record {
    let mut record = Record {
        chkdsk_exit: -1,
        chkdsk_output: String::new(),
        fs: String::new(),
        label: String::new(),
        entries: BTreeMap::new(),
    };
    for line in result.lines() {
        let fields: Vec<&str> = line.trim_end_matches('\r').split('\t').collect();
        match fields[0] {
            "chkdsk_exit" => record.chkdsk_exit = fields[1].parse().unwrap(),
            "chkdsk_output" => {
                record
                    .chkdsk_output
                    .push_str(fields.get(1).copied().unwrap_or(""));
                record.chkdsk_output.push('\n');
            }
            "fs" => record.fs = fields[1].to_string(),
            "label" => record.label = fields.get(1).copied().unwrap_or("").to_string(),
            "entry" => {
                record.entries.insert(
                    fields[6].to_string(),
                    (
                        fields[1] == "d",
                        fields[2].parse().unwrap(),
                        fields[3].to_string(),
                        fields[4].to_string(),
                        fields[5].to_string(),
                    ),
                );
            }
            _ => {}
        }
    }
    record
}

fn agree(name: &str, kind: FatKind, ours: &View, windows: &Record) {
    assert_eq!(
        windows.chkdsk_exit, 0,
        "{name}: chkdsk found problems:\n{}",
        windows.chkdsk_output
    );
    let expected_fs = if kind == FatKind::Fat32 {
        "FAT32"
    } else {
        "FAT"
    };
    assert_eq!(
        windows.fs, expected_fs,
        "{name}: the file system Windows recognised"
    );
    assert_eq!(windows.label, "WRITTEN", "{name}: label");
    let ours_names: Vec<&String> = ours.keys().collect();
    let theirs: Vec<&String> = windows.entries.keys().collect();
    assert_eq!(ours_names, theirs, "{name}: names, driver against Windows");
    for (path, (info, sha)) in ours {
        let (dir, size, write, created, hash) = &windows.entries[path];
        assert_eq!(info.is_dir, *dir, "{name}: {path}: directory");
        if !info.is_dir {
            assert_eq!(u64::from(info.size), *size, "{name}: {path}: size");
            assert_eq!(sha, hash, "{name}: {path}: contents");
        }
        assert_eq!(
            &local_time(info.modified),
            write,
            "{name}: {path}: last write time"
        );
        // Windows shows the creation time to a millisecond.
        let (seconds, millis) = info.created.to_unix().unwrap();
        let expected = format!(
            "{}.{millis:03}",
            local_time(FatTimestamp::from_unix(seconds, 0))
        );
        assert_eq!(&expected, created, "{name}: {path}: creation time");
    }
}

/// Ships the VHD to the Windows VM, checks it there, and returns what Windows
/// reported.
fn run_windows(vhd: &Path) -> Record {
    // One check at a time: two chkdsk runs at once on the guest compete for
    // shadow-copy storage and the loser reports "insufficient storage".
    let _turn = VM.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = work_dir();
    let base = vhd.file_name().unwrap().to_string_lossy().to_string();
    ssh("powershell -NoProfile -Command \"New-Item -ItemType Directory -Force fatval | Out-Null\"");
    scp(&vhd.to_string_lossy(), &format!("{}:fatval/{base}", host()));
    scp(
        &format!(
            "{}/validation/windows-check.ps1",
            env!("CARGO_MANIFEST_DIR")
        ),
        &format!("{}:fatval/windows-check.ps1", host()),
    );
    let result = format!("{base}.result");
    ssh(&format!(
        "powershell -NoProfile -ExecutionPolicy Bypass -File fatval/windows-check.ps1 -Vhd fatval/{base} -Result fatval/{result}"
    ));
    let local = dir.join(&result);
    scp(
        &format!("{}:fatval/{result}", host()),
        &local.to_string_lossy(),
    );
    ssh(&format!(
        "powershell -NoProfile -Command \"Remove-Item -Force fatval/{base}\""
    ));
    parse(&std::fs::read_to_string(&local).unwrap())
}

#[test]
fn windows_chkdsk_accepts_volumes_the_driver_wrote() {
    let dir = work_dir();
    let mut items: Vec<(String, FatKind, PathBuf)> = Vec::new();
    for kind in [FatKind::Fat12, FatKind::Fat16, FatKind::Fat32] {
        let image = dir.join(format!("{}-live.img", kind_name(kind)));
        make_disk(&image, kind);
        let snapshot = |label: &str| {
            let path = dir.join(format!("{}-{label}.vhd", kind_name(kind)));
            write_vhd(&image, &path);
            path
        };
        items.push((
            format!("{} formatted", kind_name(kind)),
            kind,
            snapshot("formatted"),
        ));
        for step in scenario() {
            let mut volume = mount(&image, OFFSET);
            (step.run)(&mut volume);
            volume.unmount().unwrap();
            if step.name == "fragmented-file" || step.name == "root-directory-growth" {
                items.push((
                    format!("{} after {}", kind_name(kind), step.name),
                    kind,
                    snapshot(step.name),
                ));
            }
        }
        // The driver's own view of each snapshot is taken from its image: the
        // snapshots differ only by the footer.
        let views: Vec<View> = items
            .iter()
            .filter(|(_, k, _)| *k == kind)
            .map(|(_, _, vhd)| {
                let copy = vhd.with_extension("check.img");
                let bytes = std::fs::read(vhd).unwrap();
                std::fs::write(&copy, &bytes[..bytes.len() - 512]).unwrap();
                view(&copy, OFFSET)
            })
            .collect();
        let start = items.len() - views.len();
        for (index, ours) in views.into_iter().enumerate() {
            let (name, _, vhd) = items[start + index].clone();
            let record = run_windows(&vhd);
            eprintln!(
                "{name}: chkdsk exit {}, {} entries",
                record.chkdsk_exit,
                record.entries.len()
            );
            agree(&name, kind, &ours, &record);
        }
    }
    let version = ssh("powershell -NoProfile -Command \"[Environment]::OSVersion.VersionString\"");
    eprintln!("Windows: {}", version.trim());
}

#[test]
fn windows_chkdsk_does_reject_damage_to_a_volume_the_driver_wrote() {
    // A control: if chkdsk accepted damaged volumes too, its verdicts above
    // would mean nothing.
    let dir = work_dir();
    let image = dir.join("damage-live.img");
    make_disk(&image, FatKind::Fat16);
    let mut volume = mount(&image, OFFSET);
    let file = volume.create_file("victim.bin").unwrap();
    volume
        .write(&file, 0, &oracle::scenario::pattern(20_000, 1))
        .unwrap();
    volume.unmount().unwrap();
    let bytes = std::fs::read(&image).unwrap();
    let volume_at = OFFSET as usize;
    let word =
        |at: usize| u16::from_le_bytes([bytes[volume_at + at], bytes[volume_at + at + 1]]) as usize;
    let (reserved, root_entries, fat_sectors, fats) =
        (word(14), word(17), word(22), bytes[volume_at + 16] as usize);
    let root = volume_at + (reserved + fats * fat_sectors) * 512;
    let entry = (0..root_entries)
        .map(|i| root + i * 32)
        .find(|at| &bytes[*at..*at + 6] == b"VICTIM")
        .expect("the file's entry");
    let first = u16::from_le_bytes([bytes[entry + 26], bytes[entry + 27]]) as usize;
    let fat0 = volume_at + reserved * 512;
    let damages: [(&str, Damage); 2] = [
        (
            "a file's chain cut short",
            Box::new(move |b| {
                b[fat0 + first * 2..fat0 + first * 2 + 2].copy_from_slice(&[0xFF, 0xFF])
            }),
        ),
        (
            "a file's size larger than its chain",
            Box::new(move |b| b[entry + 28..entry + 32].copy_from_slice(&900_000u32.to_le_bytes())),
        ),
    ];
    for (what, damage) in damages {
        let mut damaged = bytes.clone();
        damage(&mut damaged);
        let path = dir.join("damage-case.img");
        std::fs::write(&path, damaged).unwrap();
        let vhd = dir.join("damage-case.vhd");
        write_vhd(&path, &vhd);
        let record = run_windows(&vhd);
        assert_ne!(
            record.chkdsk_exit, 0,
            "chkdsk accepted a volume with damage ({what}):\n{}",
            record.chkdsk_output
        );
    }
}
