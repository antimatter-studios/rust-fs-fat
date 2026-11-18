//! Formatting FAT12, FAT16 and FAT32 volumes.
//!
//! [`plan`] turns a volume size and [`FormatOptions`] into a [`Layout`]: which
//! variant, how many sectors per cluster, how big the FATs are. [`format()`]
//! writes that layout to a [`fs_core::BlockDevice`] (at an offset within it if
//! asked), and [`format_path`] does the same into an image file, creating or
//! growing it.
//!
//! # Choosing the variant
//!
//! The variant of a FAT volume is not a field: it is decided by the number of
//! data clusters. At most 4084 is FAT12, 4085 to 65524 is FAT16, and 65525 or
//! more is FAT32. Left to choose, [`plan`] picks a variant from the size
//! (FAT12 up to 16 MiB, FAT16 below 512 MiB, FAT32 from there) and the
//! smallest cluster size that puts the cluster count inside that variant's
//! range (FAT32 uses the usual table of cluster sizes by volume size, shrunk
//! until the count reaches 65525). A variant that is requested and cannot be
//! formed at the given size is refused, not quietly turned into another.
//! Everything the result implies is checked against the reader's own geometry
//! rules before a byte is written.
//!
//! # What is written
//!
//! The reserved area, the BPB, every FAT copy (entries 0 and 1, and the root
//! cluster for FAT32), an empty root directory, and for FAT32 the FSInfo
//! sector and the backup of the boot sector and FSInfo at sectors 6 and 7.
//! The data area is not touched. A volume label given in the options is
//! written to the boot sector and to the root directory as a label entry.

use crate::boot::{BootSector, FatKind};
use crate::{Error, Result};
use fs_core::{BlockDevice, FileDevice};
use std::path::Path;

/// The FAT32 reserved sector that holds the FSInfo structure.
const FSINFO_SECTOR: u16 = 1;
/// The FAT32 reserved sector that holds the boot sector backup.
const BACKUP_SECTOR: u16 = 6;
const LARGEST_FAT12_CLUSTERS: u32 = 4084;
const LARGEST_FAT16_CLUSTERS: u32 = 65_524;
const SMALLEST_FAT32_CLUSTERS: u32 = 65_525;
const LARGEST_FAT32_CLUSTERS: u32 = 0x0FFF_FFF4;
const MIB: u64 = 1024 * 1024;

/// What to format, with everything optional left to the usual defaults.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatOptions {
    /// The variant, or `None` to choose from the size.
    pub kind: Option<FatKind>,
    /// Bytes per sector: 512, 1024, 2048 or 4096.
    pub bytes_per_sector: u16,
    /// Sectors per cluster (a power of two), or `None` to choose.
    pub sectors_per_cluster: Option<u8>,
    /// Reserved sectors: 1 for FAT12 and FAT16, 32 for FAT32 by default.
    pub reserved_sectors: Option<u16>,
    /// Copies of the FAT, 1 to 4. Two by default.
    pub fat_count: u8,
    /// Entries in the fixed root directory of FAT12 and FAT16. Must be zero
    /// or left unset for FAT32.
    pub root_entries: Option<u16>,
    /// The volume label, up to 11 characters. Written to the boot sector and
    /// to the root directory. Without one the boot sector says `NO NAME`.
    pub label: Option<String>,
    /// The volume serial number, or `None` for one derived from the clock.
    pub serial: Option<u32>,
    /// The media descriptor: 0xF0 or 0xF8 to 0xFF. 0xF8 by default.
    pub media: u8,
    /// Where the volume starts in the device, in bytes. Must be a multiple of
    /// the sector size.
    pub offset: u64,
    /// The length of the volume in bytes, or `None` for everything after
    /// `offset`.
    pub size: Option<u64>,
    /// The BPB's hidden-sector count, or `None` for `offset` in sectors.
    pub hidden_sectors: Option<u32>,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            kind: None,
            bytes_per_sector: 512,
            sectors_per_cluster: None,
            reserved_sectors: None,
            fat_count: 2,
            root_entries: None,
            label: None,
            serial: None,
            media: 0xF8,
            offset: 0,
            size: None,
            hidden_sectors: None,
        }
    }
}

/// The geometry of a volume to be formatted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    pub kind: FatKind,
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub fat_count: u8,
    /// Zero for FAT32.
    pub root_entries: u16,
    pub total_sectors: u32,
    pub sectors_per_fat: u32,
    pub cluster_count: u32,
    pub root_dir_sectors: u32,
    pub first_data_sector: u32,
    pub hidden_sectors: u32,
    pub media: u8,
    pub serial: u32,
    /// The label as stored: 11 bytes, space padded.
    pub label: [u8; 11],
    /// Whether the label was given and so also goes in the root directory.
    pub has_label: bool,
}

impl Layout {
    pub fn volume_bytes(&self) -> u64 {
        u64::from(self.total_sectors) * u64::from(self.bytes_per_sector)
    }

    pub fn cluster_bytes(&self) -> u64 {
        u64::from(self.bytes_per_sector) * u64::from(self.sectors_per_cluster)
    }
}

fn fat_bytes(kind: FatKind, entries: u64) -> u64 {
    match kind {
        FatKind::Fat12 => (entries * 3).div_ceil(2),
        FatKind::Fat16 => entries * 2,
        FatKind::Fat32 => entries * 4,
    }
}

fn cluster_range(kind: FatKind) -> (u32, u32) {
    match kind {
        FatKind::Fat12 => (1, LARGEST_FAT12_CLUSTERS),
        FatKind::Fat16 => (LARGEST_FAT12_CLUSTERS + 1, LARGEST_FAT16_CLUSTERS),
        FatKind::Fat32 => (SMALLEST_FAT32_CLUSTERS, LARGEST_FAT32_CLUSTERS),
    }
}

fn default_root_entries(total_sectors: u64) -> u16 {
    match total_sectors {
        0..=1440 => 112,
        1441..=2880 => 224,
        _ => 512,
    }
}

/// The cluster size Windows uses for a FAT32 volume of this size, in sectors
/// of 512 bytes.
fn fat32_default_cluster_bytes(volume_bytes: u64) -> u64 {
    match volume_bytes {
        0..=0x1040_0000 => 512,
        0x1040_0001..=0x2_0000_0000 => 4096,
        0x2_0000_0001..=0x4_0000_0000 => 8192,
        0x4_0000_0001..=0x8_0000_0000 => 16384,
        _ => 32768,
    }
}

/// The layout of one variant with one cluster size, if it forms: the FATs are
/// sized for the clusters left after them, and the cluster count has to fall
/// inside the variant's range.
fn layout_for(
    kind: FatKind,
    sectors_per_cluster: u8,
    total_sectors: u32,
    options: &FormatOptions,
) -> Result<Layout> {
    let bytes_per_sector = options.bytes_per_sector;
    let reserved_sectors = options.reserved_sectors.unwrap_or(match kind {
        FatKind::Fat32 => 32,
        _ => 1,
    });
    let root_entries = match kind {
        FatKind::Fat32 => 0,
        _ => options
            .root_entries
            .unwrap_or_else(|| default_root_entries(u64::from(total_sectors))),
    };
    let root_dir_sectors = (u32::from(root_entries) * 32).div_ceil(u32::from(bytes_per_sector));
    let fats = u32::from(options.fat_count);
    let mut sectors_per_fat = 1u32;
    let cluster_count = loop {
        let overhead = u64::from(reserved_sectors)
            + u64::from(fats) * u64::from(sectors_per_fat)
            + u64::from(root_dir_sectors);
        if overhead >= u64::from(total_sectors) {
            return Err(Error::Invalid(
                "the volume is too small for its own metadata",
            ));
        }
        let clusters = ((u64::from(total_sectors) - overhead) / u64::from(sectors_per_cluster))
            .min(u64::from(u32::MAX)) as u32;
        let needed = fat_bytes(kind, u64::from(clusters) + 2).div_ceil(u64::from(bytes_per_sector));
        if needed <= u64::from(sectors_per_fat) {
            break clusters;
        }
        sectors_per_fat = u32::try_from(needed)
            .map_err(|_| Error::Invalid("the FAT would not fit a 32-bit sector count"))?;
    };
    let (smallest, largest) = cluster_range(kind);
    if cluster_count < smallest {
        return Err(Error::Invalid(
            "the volume is too small for the requested variant with that cluster size",
        ));
    }
    if cluster_count > largest {
        return Err(Error::Invalid(
            "the volume is too large for the requested variant with that cluster size",
        ));
    }
    let first_data_sector = reserved_sectors as u32 + fats * sectors_per_fat + root_dir_sectors;
    let (label, has_label) = encode_label(options.label.as_deref())?;
    Ok(Layout {
        kind,
        bytes_per_sector,
        sectors_per_cluster,
        reserved_sectors,
        fat_count: options.fat_count,
        root_entries,
        total_sectors,
        sectors_per_fat,
        cluster_count,
        root_dir_sectors,
        first_data_sector,
        hidden_sectors: options
            .hidden_sectors
            .unwrap_or_else(|| (options.offset / u64::from(bytes_per_sector)) as u32),
        media: options.media,
        serial: options.serial.unwrap_or_else(clock_serial),
        label,
        has_label,
    })
}

fn clock_serial() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_secs() as u32) ^ now.subsec_nanos().rotate_left(16)
}

fn encode_label(label: Option<&str>) -> Result<([u8; 11], bool)> {
    let Some(label) = label else {
        return Ok((*b"NO NAME    ", false));
    };
    let label = label.trim_end_matches(' ');
    if label.is_empty() || label.len() > 11 {
        return Err(Error::Invalid("a volume label is 1 to 11 characters"));
    }
    let mut bytes = [b' '; 11];
    for (slot, character) in bytes.iter_mut().zip(label.chars()) {
        let upper = character.to_ascii_uppercase();
        if !upper.is_ascii() || (upper as u32) < 0x20 || "*?.,;:/\\|+=<>[]\"".contains(upper) {
            return Err(Error::Invalid(
                "a volume label has a character FAT cannot store",
            ));
        }
        *slot = upper as u8;
    }
    if label.chars().count() != label.len() {
        return Err(Error::Invalid(
            "a volume label has a character FAT cannot store",
        ));
    }
    if bytes[0] == 0xE5 {
        bytes[0] = 0x05;
    }
    Ok((bytes, true))
}

/// Works out the layout of a volume of `volume_bytes` bytes.
///
/// Refuses, with an error that says why, sizes and parameter combinations that
/// cannot form the requested variant. See the module documentation for how the
/// variant and the cluster size are chosen.
pub fn plan(volume_bytes: u64, options: &FormatOptions) -> Result<Layout> {
    if !matches!(options.bytes_per_sector, 512 | 1024 | 2048 | 4096) {
        return Err(Error::Invalid(
            "bytes per sector must be 512, 1024, 2048 or 4096",
        ));
    }
    if options.fat_count == 0 || options.fat_count > 4 {
        return Err(Error::Invalid("a volume has 1 to 4 FAT copies"));
    }
    if options.media != 0xF0 && options.media < 0xF8 {
        return Err(Error::Invalid(
            "the media descriptor must be 0xF0 or 0xF8 to 0xFF",
        ));
    }
    if options.reserved_sectors == Some(0) {
        return Err(Error::Invalid("a volume has at least one reserved sector"));
    }
    if let Some(entries) = options.root_entries {
        if !(u32::from(entries) * 32).is_multiple_of(u32::from(options.bytes_per_sector)) {
            return Err(Error::Invalid(
                "the root directory entries must fill whole sectors",
            ));
        }
        if options.kind == Some(FatKind::Fat32) && entries != 0 {
            return Err(Error::Invalid("FAT32 has no fixed root directory"));
        }
        if entries == 0 && matches!(options.kind, Some(FatKind::Fat12 | FatKind::Fat16)) {
            return Err(Error::Invalid(
                "FAT12 and FAT16 need a fixed root directory",
            ));
        }
    }
    if let Some(spc) = options.sectors_per_cluster {
        if spc == 0 || !spc.is_power_of_two() {
            return Err(Error::Invalid("sectors per cluster must be a power of two"));
        }
        if u64::from(spc) * u64::from(options.bytes_per_sector) > 65_536 {
            return Err(Error::Invalid("a cluster is at most 64 KiB"));
        }
    }
    encode_label(options.label.as_deref())?;
    let total = volume_bytes / u64::from(options.bytes_per_sector);
    let total_sectors = u32::try_from(total)
        .map_err(|_| Error::Invalid("the volume has more sectors than FAT can count"))?;
    if total_sectors < 8 {
        return Err(Error::Invalid("the volume is too small to format"));
    }
    let kinds: Vec<FatKind> = match options.kind {
        Some(kind) => vec![kind],
        None if volume_bytes <= 16 * MIB => vec![FatKind::Fat12, FatKind::Fat16],
        None if volume_bytes < 512 * MIB => vec![FatKind::Fat16, FatKind::Fat32],
        None => vec![FatKind::Fat32],
    };
    let mut refusal = Error::Invalid("the volume cannot be formatted");
    for kind in kinds {
        match plan_kind(
            kind,
            total_sectors,
            volume_bytes,
            options,
            options.kind.is_none(),
        ) {
            Ok(layout) => return Ok(layout),
            Err(error) => refusal = error,
        }
    }
    Err(refusal)
}

fn plan_kind(
    kind: FatKind,
    total_sectors: u32,
    volume_bytes: u64,
    options: &FormatOptions,
    chosen: bool,
) -> Result<Layout> {
    let bytes_per_sector = u64::from(options.bytes_per_sector);
    if let Some(spc) = options.sectors_per_cluster {
        return layout_for(kind, spc, total_sectors, options);
    }
    let mut largest = (65_536 / bytes_per_sector).min(128) as u8;
    if chosen && kind == FatKind::Fat12 {
        // A variant left to the size is not stretched over big clusters: FAT12
        // volumes that need more than 4 KiB clusters are FAT16.
        largest = largest.min((4096 / bytes_per_sector).max(1) as u8);
    }
    let mut candidates = Vec::new();
    match kind {
        FatKind::Fat32 => {
            // Start from the usual cluster size for the volume and shrink it
            // until there are enough clusters.
            let wanted = (fat32_default_cluster_bytes(volume_bytes) / bytes_per_sector).max(1);
            let mut spc = (wanted as u8).min(largest);
            loop {
                candidates.push(spc);
                if spc == 1 {
                    break;
                }
                spc /= 2;
            }
        }
        _ => {
            let mut spc = 1u8;
            while spc <= largest {
                candidates.push(spc);
                if spc == 128 {
                    break;
                }
                spc *= 2;
            }
        }
    }
    let mut refusal = None;
    for spc in candidates {
        match layout_for(kind, spc, total_sectors, options) {
            Ok(layout) => return Ok(layout),
            Err(error) => refusal = Some(error),
        }
    }
    Err(refusal.unwrap_or(Error::Invalid("the volume cannot be formatted")))
}

/// The boot sector, with the BPB, for `layout`.
pub fn boot_sector(layout: &Layout) -> Vec<u8> {
    let sector = usize::from(layout.bytes_per_sector);
    let mut boot = vec![0u8; sector];
    let fat32 = layout.kind == FatKind::Fat32;
    let code_start: usize = if fat32 { 90 } else { 62 };
    boot[0] = 0xEB;
    boot[1] = (code_start - 2) as u8;
    boot[2] = 0x90;
    boot[3..11].copy_from_slice(b"MSWIN4.1");
    boot[11..13].copy_from_slice(&layout.bytes_per_sector.to_le_bytes());
    boot[13] = layout.sectors_per_cluster;
    boot[14..16].copy_from_slice(&layout.reserved_sectors.to_le_bytes());
    boot[16] = layout.fat_count;
    boot[17..19].copy_from_slice(&layout.root_entries.to_le_bytes());
    if !fat32 && layout.total_sectors < 0x1_0000 {
        boot[19..21].copy_from_slice(&(layout.total_sectors as u16).to_le_bytes());
    }
    boot[21] = layout.media;
    if !fat32 {
        boot[22..24].copy_from_slice(&(layout.sectors_per_fat as u16).to_le_bytes());
    }
    let (per_track, heads): (u16, u16) = if layout.media == 0xF0 && layout.total_sectors <= 5760 {
        (18, 2)
    } else {
        (32, 64)
    };
    boot[24..26].copy_from_slice(&per_track.to_le_bytes());
    boot[26..28].copy_from_slice(&heads.to_le_bytes());
    boot[28..32].copy_from_slice(&layout.hidden_sectors.to_le_bytes());
    if fat32 || layout.total_sectors >= 0x1_0000 {
        boot[32..36].copy_from_slice(&layout.total_sectors.to_le_bytes());
    }
    let (extension, serial, label, kind_name) = if fat32 {
        boot[36..40].copy_from_slice(&layout.sectors_per_fat.to_le_bytes());
        boot[44..48].copy_from_slice(&2u32.to_le_bytes());
        boot[48..50].copy_from_slice(&FSINFO_SECTOR.to_le_bytes());
        boot[50..52].copy_from_slice(&BACKUP_SECTOR.to_le_bytes());
        (64, 67, 71, 82)
    } else {
        (36, 39, 43, 54)
    };
    boot[extension] = 0x80;
    boot[extension + 2] = 0x29;
    boot[serial..serial + 4].copy_from_slice(&layout.serial.to_le_bytes());
    boot[label..label + 11].copy_from_slice(&layout.label);
    boot[kind_name..kind_name + 8].copy_from_slice(match layout.kind {
        FatKind::Fat12 => b"FAT12   ",
        FatKind::Fat16 => b"FAT16   ",
        FatKind::Fat32 => b"FAT32   ",
    });
    // A boot program that says the volume is not bootable.
    const MESSAGE: &[u8] = b"This is not a bootable disk.  Please insert a bootable floppy and\r\npress any key to try again ... \r\n\0";
    let message_at = 0x7C00 + code_start + 29;
    let mut code = vec![
        0x0E,
        0x1F,
        0xBE,
        message_at as u8,
        (message_at >> 8) as u8,
        0xAC,
        0x22,
        0xC0,
        0x74,
        0x0B,
        0x56,
        0xB4,
        0x0E,
        0xBB,
        0x07,
        0x00,
        0xCD,
        0x10,
        0x5E,
        0xEB,
        0xF0,
        0x32,
        0xE4,
        0xCD,
        0x16,
        0xCD,
        0x19,
        0xEB,
        0xFE,
    ];
    code.extend_from_slice(MESSAGE);
    boot[code_start..code_start + code.len()].copy_from_slice(&code);
    // The signature is at offset 510 whatever the sector size, and also at
    // the end of a larger sector.
    boot[510..512].copy_from_slice(&[0x55, 0xAA]);
    boot[sector - 2..].copy_from_slice(&[0x55, 0xAA]);
    boot
}

/// The FAT32 FSInfo sector: every cluster but the root's is free.
pub fn fsinfo_sector(layout: &Layout) -> Vec<u8> {
    let mut info = vec![0u8; usize::from(layout.bytes_per_sector)];
    info[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    info[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    info[488..492].copy_from_slice(&(layout.cluster_count - 1).to_le_bytes());
    info[492..496].copy_from_slice(&3u32.to_le_bytes());
    info[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    info
}

/// The first bytes of a FAT: the media entry, the clean-shutdown entry, and
/// for FAT32 the end of the root directory's chain.
pub fn fat_head(layout: &Layout) -> Vec<u8> {
    let media = u32::from(layout.media);
    match layout.kind {
        FatKind::Fat12 => {
            let first = 0xF00 | media;
            vec![first as u8, ((first >> 8) as u8) | 0xF0, 0xFF]
        }
        FatKind::Fat16 => {
            let mut head = Vec::new();
            head.extend_from_slice(&(0xFF00 | media as u16).to_le_bytes());
            head.extend_from_slice(&0xFFFFu16.to_le_bytes());
            head
        }
        FatKind::Fat32 => {
            let mut head = Vec::new();
            head.extend_from_slice(&(0x0FFF_FF00 | media).to_le_bytes());
            head.extend_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
            head.extend_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
            head
        }
    }
}

/// The root directory's label entry for `layout`, timestamped now.
fn label_entry(layout: &Layout) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[..11].copy_from_slice(&layout.label);
    entry[11] = 0x08;
    let now = crate::timestamp::FatTimestamp::now();
    entry[22..24].copy_from_slice(&now.time.to_le_bytes());
    entry[24..26].copy_from_slice(&now.date.to_le_bytes());
    entry
}

/// Formats a volume on `device` as `options` say, and returns the layout it
/// wrote.
///
/// The volume occupies `options.size` bytes (the rest of the device by
/// default) from `options.offset`. Nothing is written if the layout is
/// refused. The data area is left as it was; the FATs, the root directory and
/// the reserved area are written whole.
pub fn format(device: &dyn BlockDevice, options: &FormatOptions) -> Result<Layout> {
    if !device.is_writable() {
        return Err(Error::Device(fs_core::Error::ReadOnly));
    }
    if !options
        .offset
        .is_multiple_of(u64::from(options.bytes_per_sector))
    {
        return Err(Error::Invalid(
            "the volume offset must be a multiple of the sector size",
        ));
    }
    let available = device
        .size_bytes()
        .checked_sub(options.offset)
        .ok_or(Error::Invalid("the volume offset is beyond the device"))?;
    let volume_bytes = options.size.unwrap_or(available);
    if volume_bytes > available {
        return Err(Error::Invalid("the volume is larger than the device"));
    }
    let layout = plan(volume_bytes, options)?;
    // The reader's own rules must agree with what is about to be written.
    let boot = boot_sector(&layout);
    let parsed = BootSector::parse(&boot)?;
    if !parsed.signature_ok
        || parsed.kind != layout.kind
        || parsed.cluster_count != layout.cluster_count
        || parsed.first_data_sector != layout.first_data_sector
    {
        return Err(Error::Invalid(
            "the planned layout does not read back as itself",
        ));
    }

    let sector = u64::from(layout.bytes_per_sector);
    let start = options.offset;
    let write = |at: u64, bytes: &[u8]| -> Result<()> {
        device.write_at(start + at, bytes)?;
        Ok(())
    };
    let zeros = vec![0u8; 1 << 20];
    let zero = |from: u64, length: u64| -> Result<()> {
        let mut done = 0;
        while done < length {
            let chunk = (length - done).min(zeros.len() as u64) as usize;
            write(from + done, &zeros[..chunk])?;
            done += chunk as u64;
        }
        Ok(())
    };

    zero(0, u64::from(layout.reserved_sectors) * sector)?;
    write(0, &boot)?;
    if layout.kind == FatKind::Fat32 {
        let info = fsinfo_sector(&layout);
        write(u64::from(FSINFO_SECTOR) * sector, &info)?;
        write(u64::from(BACKUP_SECTOR) * sector, &boot)?;
        write(u64::from(BACKUP_SECTOR + 1) * sector, &info)?;
        // The extra boot-code sectors carry the signature too.
        let mut trailer = vec![0u8; sector as usize];
        trailer[510..512].copy_from_slice(&[0x55, 0xAA]);
        trailer[sector as usize - 2..].copy_from_slice(&[0x55, 0xAA]);
        write(2 * sector, &trailer)?;
        write(u64::from(BACKUP_SECTOR + 2) * sector, &trailer)?;
    }
    let head = fat_head(&layout);
    for copy in 0..u64::from(layout.fat_count) {
        let fat = (u64::from(layout.reserved_sectors) + copy * u64::from(layout.sectors_per_fat))
            * sector;
        zero(fat, u64::from(layout.sectors_per_fat) * sector)?;
        write(fat, &head)?;
    }
    let root_start = (u64::from(layout.reserved_sectors)
        + u64::from(layout.fat_count) * u64::from(layout.sectors_per_fat))
        * sector;
    let root_length = if layout.kind == FatKind::Fat32 {
        layout.cluster_bytes()
    } else {
        u64::from(layout.root_dir_sectors) * sector
    };
    zero(root_start, root_length)?;
    if layout.has_label {
        write(root_start, &label_entry(&layout))?;
    }
    device.flush()?;
    Ok(layout)
}

/// Formats an image file or device node at `path`.
///
/// A file that does not exist is created and given `options.size` bytes (plus
/// `options.offset`), which is then required. A file shorter than the volume
/// needs is extended.
pub fn format_path(path: &Path, options: &FormatOptions) -> Result<Layout> {
    let existing = std::fs::metadata(path).ok().map(|metadata| metadata.len());
    if existing.is_none() || options.size.is_some() {
        let size = options
            .size
            .ok_or(Error::Invalid("a new image file needs a size"))?;
        let wanted = options
            .offset
            .checked_add(size)
            .ok_or(Error::Invalid("the volume extends beyond the largest file"))?;
        if existing.is_none_or(|length| length < wanted) {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .map_err(|error| Error::Device(fs_core::Error::Io(error)))?;
            file.set_len(wanted)
                .map_err(|error| Error::Device(fs_core::Error::Io(error)))?;
        }
    }
    let device = FileDevice::open_rw(path)?;
    format(&device, options)
}
