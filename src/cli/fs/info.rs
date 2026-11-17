//! `info`: the boot sector and the variant, read as raw bytes.
//!
//! This goes through `RawVolume`, which needs no valid boot sector, so it
//! answers for a damaged one: it reports every field as the bytes say it,
//! whether the library's checked layer accepts them and, if not, why, and
//! where a backup copy is and whether that one holds up.

use super::target::{failure, Target};
use super::{hex, Verb};
use fs_core::cli::clap::{ArgMatches, Command};
use fs_core::cli::{CliError, Json, Outcome};
use fs_fat32::volume::VolumeAPI;
use fs_fat32::{BootSector, FatKind};

pub const INFO: Verb = Verb {
    name: "info",
    command,
    run,
};

const SECTOR: usize = 512;
/// Where mkfs tools put the FAT32 backup boot sector, in the sectors of the
/// volume; used when sector 0 is too damaged to say.
const CONVENTIONAL_BACKUP: u64 = 6;

fn command() -> Command {
    Command::new("info")
        .about("Report the boot sector and the variant, even when the boot sector is damaged")
        .after_help(
            "Reads the boot sector as raw bytes. `valid` says whether the library accepts it; when it \
             does not, `problem` says why. For a FAT32 layout, or a sector 0 that cannot be read as \
             any layout, the backup copy is checked too, and `suggest` gives the --boot-sector \
             argument that makes the other verbs use it.",
        )
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

/// Printable text of a fixed-width field, padding and NULs trimmed: any other
/// byte that is not printable ASCII is shown as `?`.
fn text(bytes: &[u8]) -> String {
    let shown: String = bytes
        .iter()
        .map(|byte| match byte {
            0x20..=0x7e => *byte as char,
            0 => ' ',
            _ => '?',
        })
        .collect();
    shown.trim_end().to_string()
}

fn variant(kind: FatKind) -> &'static str {
    match kind {
        FatKind::Fat12 => "FAT12",
        FatKind::Fat16 => "FAT16",
        FatKind::Fat32 => "FAT32",
    }
}

/// The fields as the bytes say them, whatever the library thinks of them.
fn fields(sector: &[u8; SECTOR]) -> (bool, Vec<(&'static str, Json)>) {
    // Which extended layout applies is not knowable without the variant, and
    // the variant comes from the geometry. The one thing that separates the
    // layouts on disk is the 16-bit sectors-per-FAT field: FAT32 leaves it 0.
    let fat32_layout = u16_at(sector, 22) == 0 && u32_at(sector, 36) != 0;
    let mut out = vec![
        ("oem", Json::from(text(&sector[3..11]))),
        ("bytes_per_sector", Json::from(u16_at(sector, 11))),
        ("sectors_per_cluster", Json::from(u64::from(sector[13]))),
        ("reserved_sectors", Json::from(u16_at(sector, 14))),
        ("fat_count", Json::from(u64::from(sector[16]))),
        ("root_entries", Json::from(u16_at(sector, 17))),
        ("total_sectors_16", Json::from(u16_at(sector, 19))),
        ("media", Json::from(format!("0x{:02x}", sector[21]))),
        ("sectors_per_fat_16", Json::from(u16_at(sector, 22))),
        ("hidden_sectors", Json::from(u32_at(sector, 28))),
        ("total_sectors_32", Json::from(u32_at(sector, 32))),
        ("signature", Json::from(hex(&sector[510..512]))),
        (
            "signature_valid",
            Json::from(sector[510..512] == [0x55, 0xAA]),
        ),
        (
            "layout",
            Json::from(if fat32_layout { "fat32" } else { "fat12/16" }),
        ),
    ];
    if fat32_layout {
        out.extend([
            ("sectors_per_fat_32", Json::from(u32_at(sector, 36))),
            ("mirroring_flags", Json::from(u16_at(sector, 40))),
            ("version", Json::from(u16_at(sector, 42))),
            ("root_cluster", Json::from(u32_at(sector, 44))),
            ("fsinfo_sector", Json::from(u16_at(sector, 48))),
            ("backup_boot_sector", Json::from(u16_at(sector, 50))),
            (
                "volume_serial",
                Json::from(format!("0x{:08x}", u32_at(sector, 67))),
            ),
            ("volume_label", Json::from(text(&sector[71..82]))),
            ("fs_type_label", Json::from(text(&sector[82..90]))),
        ]);
    } else {
        out.extend([
            (
                "volume_serial",
                Json::from(format!("0x{:08x}", u32_at(sector, 39))),
            ),
            ("volume_label", Json::from(text(&sector[43..54]))),
            ("fs_type_label", Json::from(text(&sector[54..62]))),
        ]);
    }
    (fat32_layout, out)
}

/// What the library's checked layer makes of a boot sector, for a device of
/// `image_bytes`: the geometry, or the reason it is refused.
fn verdict(sector: &[u8; SECTOR], image_bytes: u64) -> (Option<BootSector>, Option<String>) {
    match BootSector::parse(sector) {
        Err(error) => (None, Some(error.to_string())),
        Ok(boot) => {
            let bytes = u64::from(boot.total_sectors) * u64::from(boot.bytes_per_sector);
            if bytes > image_bytes {
                (
                    Some(boot),
                    Some("invalid FAT volume: BPB volume length exceeds device".to_string()),
                )
            } else {
                (Some(boot), None)
            }
        }
    }
}

fn geometry(boot: &BootSector, image_bytes: u64) -> Json {
    let volume_bytes = u64::from(boot.total_sectors) * u64::from(boot.bytes_per_sector);
    Json::object([
        ("variant", Json::from(variant(boot.kind))),
        ("cluster_count", Json::from(boot.cluster_count)),
        ("cluster_bytes", Json::from(boot.cluster_size())),
        ("total_sectors", Json::from(boot.total_sectors)),
        ("volume_bytes", Json::from(volume_bytes)),
        ("fits_device", Json::from(volume_bytes <= image_bytes)),
        ("sectors_per_fat", Json::from(boot.sectors_per_fat)),
        ("active_fat", Json::from(u64::from(boot.active_fat))),
        ("first_data_sector", Json::from(boot.first_data_sector)),
        ("root_dir_sectors", Json::from(boot.root_dir_sectors)),
        ("root_cluster", Json::from(boot.root_cluster)),
        (
            "volume_serial",
            Json::from(format!("0x{:08x}", boot.volume_serial)),
        ),
    ])
}

fn run(target: &Target, _: &ArgMatches) -> Result<Outcome, CliError> {
    let raw = target.raw()?;
    let image_bytes = raw.size_bytes();
    let selected = target.boot_sector.unwrap_or(0);
    let mut sector = [0u8; SECTOR];
    raw.read_volume_bytes(selected * SECTOR as u64, &mut sector)
        .map_err(|error| {
            CliError::failed(format!(
                "cannot read a boot sector at sector {selected}: {error}"
            ))
        })?;

    let (fat32_layout, boot_fields) = fields(&sector);
    let (boot, problem) = verdict(&sector, image_bytes);
    let valid = problem.is_none();

    // The backup copy. A FAT32 sector 0 says where it is; a sector 0 that is
    // not recognisably any layout cannot, so the conventional place is read.
    let mut backup = Json::Null;
    if fat32_layout || boot.is_none() {
        let (location, backup_sector) = match (fat32_layout, u16_at(&sector, 50)) {
            (true, field) if field != 0 && field != 0xFFFF => ("bpb", u64::from(field)),
            _ => ("conventional", CONVENTIONAL_BACKUP),
        };
        let bytes_per_sector = match u16_at(&sector, 11) {
            size @ (512 | 1024 | 2048 | 4096) => u64::from(size),
            _ => SECTOR as u64,
        };
        let byte_offset = backup_sector * bytes_per_sector;
        let mut copy = [0u8; SECTOR];
        let read = raw.read_volume_bytes(byte_offset, &mut copy);
        let mut report = vec![
            ("location", Json::from(location)),
            ("sector", Json::from(backup_sector)),
            ("byte_offset", Json::from(byte_offset)),
        ];
        match read {
            Err(error) => {
                report.push(("accepted", Json::from(false)));
                report.push(("problem", Json::from(failure(error).message)));
            }
            Ok(()) => {
                let (copy_boot, copy_problem) = verdict(&copy, image_bytes);
                let accepted = copy_problem.is_none();
                report.push(("accepted", Json::from(accepted)));
                report.push(("problem", Json::from(copy_problem)));
                report.push(("identical_to_selected", Json::from(copy == sector)));
                if let Some(copy_boot) = &copy_boot {
                    report.push(("variant", Json::from(variant(copy_boot.kind))));
                }
                if accepted && !valid {
                    report.push((
                        "suggest",
                        Json::from(format!("--boot-sector {}", byte_offset / SECTOR as u64)),
                    ));
                }
            }
        }
        backup = Json::object(report);
    }

    Ok(Outcome::report(Json::object([
        ("image_bytes", Json::from(image_bytes)),
        ("offset", Json::from(target.offset)),
        ("boot_sector_read_from", Json::from(selected)),
        ("valid", Json::from(valid)),
        ("problem", Json::from(problem)),
        (
            "volume",
            match &boot {
                Some(boot) => geometry(boot, image_bytes),
                None => Json::Null,
            },
        ),
        ("boot_sector", Json::object(boot_fields)),
        ("backup", backup),
    ])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_sector_is_not_recognised_as_fat32_and_has_no_variant() {
        let sector = [0u8; SECTOR];
        let (fat32, fields) = fields(&sector);
        assert!(!fat32);
        assert!(fields
            .iter()
            .any(|(key, value)| *key == "signature_valid" && *value == Json::from(false)));
        let (boot, problem) = verdict(&sector, 1 << 20);
        assert!(boot.is_none());
        assert!(problem.unwrap().contains("missing boot-sector signature"));
    }

    #[test]
    fn field_text_trims_padding_and_marks_other_bytes() {
        assert_eq!(text(b"mkfs.fat"), "mkfs.fat");
        assert_eq!(text(b"NO NAME    "), "NO NAME");
        assert_eq!(text(&[0, 0, 0]), "");
        assert_eq!(text(&[b'A', 0xFF, b'B']), "A?B");
    }
}
