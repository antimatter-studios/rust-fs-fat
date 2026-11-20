//! The three API layers of `fs_core::layers`, implemented for FAT12, FAT16
//! and FAT32.
//!
//! [`Volume`] implements `VolumeAPI`, `OpenVolume`, `PrimitiveAPI` and
//! `FileAPI`. What is specific to FAT stays here, below the traits: the boot
//! sector and its FAT32 backup, both FAT copies, cluster chains, directory
//! entries with their long names and code page 437 short names. The salvage
//! verbs are [`FatAdapter`], which holds geometry and never a device.
//!
//! ```no_run
//! use fs_core::layers::{FileAPI, OpenVolume, PrimitiveAPI, VolumeAPI};
//! use fs_core::FileDevice;
//! use fs_fat32::Volume;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let volume = Volume::open_for_salvage(FileDevice::open("disk.img")?)?;
//! println!("{}: {:?}", volume.format(), volume.health()?.state);
//! let candidates = volume.salvage()?.all_candidates()?;
//! # let _ = candidates;
//! # Ok(()) }
//! ```
//!
//! Nothing here writes: the volume holds a `BlockRead`, which has no write
//! method. Times are read as UTC, as [`crate::timestamp`] does, because a
//! FAT time carries no zone.

mod adapter;
mod damage;

pub use adapter::{FatAdapter, FatDetail};

use crate::boot::{BootSector, FatKind};
use crate::directory::{oem_text, scan_records};
use crate::primitives::{classify_fat_value, FatValue};
use crate::timestamp::FatTimestamp;
use crate::volume::Volume;
use crate::{Error, Result};
use fs_core::layers::{
    check_bounds, ApiError, Capabilities, ClusterLayout, Damage, DamageKind, DirEntry, Extent,
    FileAPI, Geometry, Integrity, Metadata, NodeKind, OpenVolume, PrimitiveAPI, Probe, Recognition,
    SalvageError, Severity, Structure, StructureRole, SuperblockCopy, Timestamp, VolumeAPI,
    VolumeHealth, VolumeSupport,
};
use fs_core::BlockRead;

/// A file or directory on a FAT volume: the directory entry that names it.
///
/// It belongs to the volume that issued it. A directory entry is all FAT
/// has, so the node carries it whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FatNode {
    kind: NodeKind,
    first_cluster: u32,
    size: u32,
    attributes: u8,
    /// The 32-byte short entry, for the times. Zeros for the root.
    raw: [u8; 32],
    root: bool,
}

impl FatNode {
    /// The first cluster of the node's data or directory; 0 for the fixed
    /// root directory of FAT12 and FAT16, and for an empty file.
    pub fn first_cluster(&self) -> u32 {
        self.first_cluster
    }

    /// The attribute byte of the directory entry.
    pub fn attributes(&self) -> u8 {
        self.attributes
    }
}

impl<Device: BlockRead> VolumeAPI for Volume<Device> {
    type Error = Error;
    type Superblock = BootSector;

    fn format(&self) -> &'static str {
        kind_name(self.boot().kind)
    }

    fn size_bytes(&self) -> u64 {
        self.len()
    }

    fn read_volume_bytes(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        check_bounds(offset, output.len(), self.len())?;
        self.device().read_at(offset, output)?;
        Ok(())
    }

    fn geometry(&self) -> Result<Geometry> {
        let boot = self.boot();
        let sector = u64::from(boot.bytes_per_sector);
        Ok(Geometry {
            sector_size: u32::from(boot.bytes_per_sector),
            allocation_unit: boot.cluster_size(),
            allocation_units: u64::from(boot.cluster_count),
            declared_bytes: u64::from(boot.total_sectors) * sector,
            layout: Some(cluster_layout(boot)),
            label: self.label(),
            serial: Some(format!(
                "{:04X}-{:04X}",
                boot.volume_serial >> 16,
                boot.volume_serial & 0xFFFF
            )),
        })
    }

    fn superblock(&self) -> Result<BootSector> {
        Ok(self.boot().clone())
    }

    fn superblocks(&self) -> Result<Vec<SuperblockCopy<BootSector>>> {
        Ok(self.boot_copies())
    }

    fn support(&self) -> Result<VolumeSupport> {
        Ok(VolumeSupport {
            files: self.read_directory(self.root_first()).is_ok(),
            salvage: adapter::capabilities(),
            unsupported: Vec::new(),
        })
    }

    fn health(&self) -> Result<VolumeHealth> {
        let adapter = FatAdapter::new(self.boot().clone(), self.boot_sector_offset());
        let mut findings = adapter.health_findings(self.device(), self.len());
        if let Err(error) = self.read_directory(self.root_first()) {
            findings.push(Damage {
                kind: DamageKind::Broken,
                severity: Severity::Error,
                location: adapter.root_extent(),
                structure: "root directory".into(),
                detail: format!("the root directory cannot be read: {error}"),
            });
        }
        Ok(VolumeHealth::from_findings(findings))
    }
}

impl<Device: BlockRead> OpenVolume for Volume<Device> {
    type Source = Device;

    fn probe(source: &dyn BlockRead) -> std::result::Result<Probe, fs_core::Error> {
        let size = source.size_bytes();
        let none = |detail: &str| Probe {
            format: "fat32",
            recognition: Recognition::No,
            declared_bytes: None,
            detail: detail.to_owned(),
        };
        if size < 512 {
            return Ok(none("the source is shorter than a boot sector"));
        }
        let mut sector = [0u8; 512];
        source.read_at(0, &mut sector)?;
        let declared = |boot: &BootSector| {
            Some(u64::from(boot.total_sectors) * u64::from(boot.bytes_per_sector))
        };
        match BootSector::parse(&sector) {
            Ok(boot) if boot.signature_ok => Ok(Probe {
                format: kind_name(boot.kind),
                recognition: Recognition::Yes,
                declared_bytes: declared(&boot),
                detail: format!("a {} boot sector", kind_name(boot.kind)),
            }),
            Ok(boot) => Ok(Probe {
                format: kind_name(boot.kind),
                recognition: Recognition::Damaged,
                declared_bytes: declared(&boot),
                detail: "the BPB decodes, but the 0x55 0xAA signature is missing".into(),
            }),
            Err(error) => {
                let named = type_label(&sector);
                let backup = crate::volume::backup_boot(source, &sector);
                match (backup, named) {
                    (Some((boot, at)), _) => Ok(Probe {
                        format: kind_name(boot.kind),
                        recognition: Recognition::Damaged,
                        declared_bytes: declared(&boot),
                        detail: format!(
                            "the boot sector does not decode ({error}); the backup at byte {at} does"
                        ),
                    }),
                    (None, Some(format)) => Ok(Probe {
                        format,
                        recognition: Recognition::Damaged,
                        declared_bytes: None,
                        detail: format!(
                            "the boot sector names {format} but does not decode ({error})"
                        ),
                    }),
                    (None, None) => Ok(none("no FAT boot sector, and no backup")),
                }
            }
        }
    }

    fn open(source: Device) -> Result<Self> {
        Volume::open(source)
    }

    fn open_for_salvage(source: Device) -> Result<Self> {
        Volume::open_salvage(source)
    }
}

impl<Device: BlockRead> PrimitiveAPI for Volume<Device> {
    type Adapter = FatAdapter;
    type Hypothesis = ();

    fn source(&self) -> &dyn BlockRead {
        self.device()
    }

    fn adapter(&self) -> std::result::Result<FatAdapter, SalvageError> {
        Ok(FatAdapter::new(
            self.boot().clone(),
            self.boot_sector_offset(),
        ))
    }

    fn structures(&self) -> std::result::Result<Vec<Structure>, SalvageError> {
        Ok(self.structure_list())
    }
}

impl<Device: BlockRead> FileAPI for Volume<Device> {
    type Node = FatNode;

    fn root(&self) -> Result<FatNode> {
        Ok(FatNode {
            kind: NodeKind::Directory,
            first_cluster: self.root_first(),
            size: 0,
            attributes: 0x10,
            raw: [0; 32],
            root: true,
        })
    }

    fn read_dir(&self, directory: &FatNode) -> Result<Vec<DirEntry<FatNode>>> {
        if directory.kind != NodeKind::Directory {
            return Err(
                ApiError::NotADirectory(format!("cluster {}", directory.first_cluster)).into(),
            );
        }
        let bytes = self.read_directory(directory.first_cluster)?;
        let mut entries = Vec::new();
        for scanned in scan_records(&bytes, self.boot().kind, false)? {
            let record = scanned.record;
            let entry = record.entry;
            let listed = !entry.deleted
                && !entry.name.is_empty()
                && entry.name != "."
                && entry.name != ".."
                && !entry.name.contains(['/', '\0']);
            if !listed {
                continue;
            }
            let kind = if entry.attributes & 0x10 != 0 {
                NodeKind::Directory
            } else {
                NodeKind::File
            };
            entries.push(DirEntry {
                bytes: entry.name.clone().into_bytes(),
                lossy: false,
                kind,
                size: if kind == NodeKind::File {
                    u64::from(entry.size)
                } else {
                    0
                },
                node: FatNode {
                    kind,
                    first_cluster: entry.first_cluster,
                    size: if kind == NodeKind::File {
                        entry.size
                    } else {
                        0
                    },
                    attributes: entry.attributes,
                    raw: record.raw,
                    root: false,
                },
                name: entry.name,
            });
        }
        Ok(entries)
    }

    fn metadata(&self, node: &FatNode) -> Result<Metadata> {
        let mut metadata = Metadata::new(node.kind, u64::from(node.size));
        if node.root {
            return Ok(metadata);
        }
        metadata.attributes = Some(u64::from(node.attributes));
        if node.kind == NodeKind::File {
            metadata.allocated = Some(
                u64::from(node.size).div_ceil(self.boot().cluster_size())
                    * self.boot().cluster_size(),
            );
        }
        let raw = &node.raw;
        let time = |date: u16, time: u16, tenths: u8| {
            FatTimestamp { date, time, tenths }
                .to_unix()
                .map(|(seconds, millis)| Timestamp {
                    seconds,
                    nanoseconds: millis * 1_000_000,
                })
        };
        let word = |at: usize| u16::from_le_bytes([raw[at], raw[at + 1]]);
        metadata.created = time(word(16), word(14), raw[13]);
        metadata.modified = time(word(24), word(22), 0);
        metadata.accessed = time(word(18), 0, 0);
        Ok(metadata)
    }

    fn read_at(&self, file: &FatNode, offset: u64, output: &mut [u8]) -> Result<usize> {
        if file.kind == NodeKind::Directory {
            return Err(ApiError::IsADirectory(format!("cluster {}", file.first_cluster)).into());
        }
        let length = u64::from(file.size);
        if offset >= length || output.is_empty() {
            return Ok(0);
        }
        let count = (length - offset).min(output.len() as u64) as usize;
        let cluster_size = self.boot().cluster_size();
        let mut walker = ChainWalker::new(self);
        let mut cluster = walker.nth(file.first_cluster, offset / cluster_size)?;
        let mut index = offset / cluster_size;
        let mut copied = 0;
        while copied < count {
            let position = offset + copied as u64;
            let within = position % cluster_size;
            let chunk = (cluster_size - within).min((count - copied) as u64) as usize;
            self.read_bytes(
                self.cluster_offset(cluster)? + within,
                &mut output[copied..copied + chunk],
            )?;
            copied += chunk;
            if copied < count {
                index += 1;
                cluster = walker.next(cluster, index)?;
            }
        }
        Ok(count)
    }

    /// FAT compares names without regard to case.
    fn names_match(&self, stored: &str, wanted: &str) -> bool {
        stored == wanted || stored.to_uppercase() == wanted.to_uppercase()
    }
}

fn kind_name(kind: FatKind) -> &'static str {
    match kind {
        FatKind::Fat12 => "fat12",
        FatKind::Fat16 => "fat16",
        FatKind::Fat32 => "fat32",
    }
}

/// The file system type a boot sector names, in the informational field
/// formatters fill in: at 54 for FAT12 and FAT16, at 82 for FAT32.
fn type_label(sector: &[u8; 512]) -> Option<&'static str> {
    [(&sector[54..62], false), (&sector[82..90], true)]
        .into_iter()
        .find_map(|(text, _)| match text {
            b"FAT12   " => Some("fat12"),
            b"FAT16   " => Some("fat16"),
            b"FAT32   " => Some("fat32"),
            _ => None,
        })
}

pub(crate) fn cluster_layout(boot: &BootSector) -> ClusterLayout {
    ClusterLayout {
        first: 2,
        count: u64::from(boot.cluster_count),
        size: boot.cluster_size(),
        offset: u64::from(boot.first_data_sector) * u64::from(boot.bytes_per_sector),
    }
}

/// Follows a cluster chain for a read, one buffered window of the active FAT
/// at a time, so reaching the cluster at index `n` costs a pass over the FAT
/// bytes in between rather than a device read per entry.
struct ChainWalker<'a, Device: BlockRead> {
    volume: &'a Volume<Device>,
    window: Vec<u8>,
    start: u64,
    /// Brent's cycle detection over the clusters stepped through: a chain
    /// that comes back to a cluster it has been at is refused, not read
    /// again as though it were more of the file.
    tortoise: u32,
    power: u64,
    lambda: u64,
}

const WINDOW: u64 = 64 * 1024;

impl<'a, Device: BlockRead> ChainWalker<'a, Device> {
    fn new(volume: &'a Volume<Device>) -> Self {
        Self {
            volume,
            window: Vec::new(),
            start: 0,
            tortoise: 0,
            power: 1,
            lambda: 0,
        }
    }

    /// The cluster at `index` of the chain that starts at `first`.
    fn nth(&mut self, first: u32, index: u64) -> Result<u32> {
        let boot = self.volume.boot();
        if first < 2 || first >= boot.cluster_count + 2 {
            return Err(Error::Invalid("file chain starts outside the volume"));
        }
        let mut cluster = first;
        self.tortoise = first;
        self.power = 1;
        self.lambda = 0;
        for step in 1..=index {
            cluster = self.next(cluster, step)?;
        }
        Ok(cluster)
    }

    /// The cluster after `cluster`, which is chain index `index - 1`.
    fn next(&mut self, cluster: u32, index: u64) -> Result<u32> {
        let boot = self.volume.boot();
        if index > u64::from(boot.cluster_count) {
            return Err(Error::Invalid("file chain loops"));
        }
        match classify_fat_value(boot.kind, self.entry(cluster)?, boot.cluster_count + 1) {
            FatValue::Next(next) => {
                if next == self.tortoise {
                    return Err(Error::Invalid("file chain loops"));
                }
                self.lambda += 1;
                if self.lambda == self.power {
                    self.tortoise = next;
                    self.power *= 2;
                    self.lambda = 0;
                }
                Ok(next)
            }
            _ => Err(Error::Invalid("file chain ends before file size")),
        }
    }

    fn entry(&mut self, cluster: u32) -> Result<u32> {
        let boot = self.volume.boot();
        let fat_start = (u64::from(boot.reserved_sectors)
            + u64::from(boot.active_fat) * u64::from(boot.sectors_per_fat))
            * u64::from(boot.bytes_per_sector);
        let fat_size = u64::from(boot.sectors_per_fat) * u64::from(boot.bytes_per_sector);
        let at = match boot.kind {
            FatKind::Fat12 => u64::from(cluster) * 3 / 2,
            FatKind::Fat16 => u64::from(cluster) * 2,
            FatKind::Fat32 => u64::from(cluster) * 4,
        };
        let width = if boot.kind == FatKind::Fat32 { 4 } else { 2 };
        if at + width > fat_size {
            return Err(Error::Bounds);
        }
        let inside = at >= self.start && at + width <= self.start + self.window.len() as u64;
        if !inside {
            let start = at / WINDOW * WINDOW;
            let length = (WINDOW + 4).min(fat_size - start);
            self.window = vec![0; length as usize];
            self.start = start;
            self.volume
                .read_bytes(fat_start + start, &mut self.window)?;
        }
        let at = (at - self.start) as usize;
        let pair = &self.window[at..at + width as usize];
        Ok(match boot.kind {
            FatKind::Fat12 => {
                let word = u16::from_le_bytes([pair[0], pair[1]]);
                if cluster & 1 == 0 {
                    u32::from(word & 0x0FFF)
                } else {
                    u32::from(word >> 4)
                }
            }
            FatKind::Fat16 => u32::from(u16::from_le_bytes([pair[0], pair[1]])),
            FatKind::Fat32 => u32::from_le_bytes(pair.try_into().unwrap()) & 0x0FFF_FFFF,
        })
    }
}

impl<Device: BlockRead> Volume<Device> {
    /// Where the root directory starts: its first cluster on FAT32, and 0
    /// for the fixed root area of FAT12 and FAT16.
    fn root_first(&self) -> u32 {
        if self.boot().kind == FatKind::Fat32 {
            self.boot().root_cluster
        } else {
            0
        }
    }

    /// A directory's bytes: the fixed root area when `first` is 0, otherwise
    /// its cluster chain, read up to the cluster that holds the end marker.
    pub(crate) fn read_directory(&self, first: u32) -> Result<Vec<u8>> {
        let boot = self.boot();
        if first == 0 && boot.kind != FatKind::Fat32 {
            let start = u64::from(boot.reserved_sectors)
                + u64::from(boot.fat_count) * u64::from(boot.sectors_per_fat);
            let mut bytes = vec![
                0;
                usize::try_from(
                    u64::from(boot.root_dir_sectors) * u64::from(boot.bytes_per_sector)
                )
                .map_err(|_| Error::Bounds)?
            ];
            self.read_bytes(start * u64::from(boot.bytes_per_sector), &mut bytes)?;
            return Ok(bytes);
        }
        let mut output = Vec::new();
        let mut cluster = first;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..boot.cluster_count {
            if cluster < 2 || cluster >= boot.cluster_count + 2 {
                return Err(Error::Invalid("directory chain leaves the volume"));
            }
            if !seen.insert(cluster) {
                return Err(Error::Invalid("directory chain loops"));
            }
            let mut data = vec![0; boot.cluster_size() as usize];
            self.read_cluster(cluster, &mut data)?;
            let ended = data.chunks_exact(32).any(|entry| entry[0] == 0);
            output.extend(data);
            if ended {
                return Ok(output);
            }
            match self.fat_value(cluster)? {
                FatValue::Next(next) => cluster = next,
                FatValue::EndOfChain => return Ok(output),
                _ => return Err(Error::Invalid("directory chain is damaged")),
            }
        }
        Err(Error::Invalid("directory chain loops"))
    }

    /// The volume label: the label entry of the root directory, which is
    /// what Windows shows, else the boot sector's label field.
    fn label(&self) -> Option<String> {
        let boot = self.boot();
        let from_root = self
            .read_directory(self.root_first())
            .ok()
            .and_then(|bytes| {
                bytes
                    .chunks_exact(32)
                    .take_while(|entry| entry[0] != 0)
                    .find(|entry| entry[0] != 0xE5 && entry[11] & 0x58 == 0x08 && entry[11] != 0x0F)
                    .map(|entry| oem_text(&entry[..11]))
            });
        let label = from_root.or_else(|| {
            let at = self.boot_sector_offset() + if boot.kind == FatKind::Fat32 { 71 } else { 43 };
            let mut field = [0u8; 11];
            self.read_bytes(at, &mut field).ok()?;
            Some(oem_text(&field))
        })?;
        (!label.is_empty() && label != "NO NAME").then_some(label)
    }

    /// The raw boot sector at `offset`, when it can be read.
    fn boot_bytes(&self, offset: u64) -> Option<[u8; 512]> {
        let mut sector = [0u8; 512];
        (offset + 512 <= self.len()).then_some(())?;
        self.read_bytes(offset, &mut sector).ok()?;
        Some(sector)
    }

    /// The primary boot sector and the FAT32 backup, each decoded where it
    /// can be, with the signature as the only check a boot sector has.
    fn boot_copies(&self) -> Vec<SuperblockCopy<BootSector>> {
        let boot = self.boot();
        let sector = u64::from(boot.bytes_per_sector);
        let decode = |copy: u32, offset: u64| {
            let parsed = self
                .boot_bytes(offset)
                .and_then(|bytes| BootSector::parse(&bytes).ok());
            SuperblockCopy {
                copy,
                location: Extent::new(offset, sector),
                integrity: match &parsed {
                    Some(boot) if boot.signature_ok => Integrity::Verified,
                    _ => Integrity::Failed,
                },
                superblock: parsed,
            }
        };
        let mut copies = vec![decode(0, 0)];
        if boot.kind == FatKind::Fat32 {
            if let Some(backup) = self.backup_sector() {
                copies.push(decode(1, u64::from(backup) * sector));
            }
        }
        copies
    }

    /// The sector of the FAT32 backup boot sector, from the boot sector the
    /// volume was read from; `None` when the volume says it has none.
    fn backup_sector(&self) -> Option<u16> {
        let bytes = self.boot_bytes(self.boot_sector_offset())?;
        let sector = u16::from_le_bytes([bytes[50], bytes[51]]);
        (sector != 0 && sector != 0xFFFF).then_some(sector)
    }

    /// The FSInfo sector of a FAT32 volume.
    fn fsinfo_sector(&self) -> Option<u16> {
        let bytes = self.boot_bytes(self.boot_sector_offset())?;
        let sector = u16::from_le_bytes([bytes[48], bytes[49]]);
        (sector != 0 && sector != 0xFFFF).then_some(sector)
    }

    fn structure_list(&self) -> Vec<Structure> {
        let boot = self.boot();
        let sector = u64::from(boot.bytes_per_sector);
        let mut structures = Vec::new();
        for copy in self.boot_copies() {
            structures.push(Structure {
                name: if copy.copy == 0 {
                    "boot sector".into()
                } else {
                    "backup boot sector".into()
                },
                role: StructureRole::Boot,
                location: copy.location,
                copy: copy.copy,
                integrity: copy.integrity,
            });
        }
        if let Some(info) = self.fsinfo_sector().filter(|_| boot.kind == FatKind::Fat32) {
            let at = u64::from(info) * sector;
            let mut bytes = [0u8; 512];
            let good = self.read_bytes(at, &mut bytes).is_ok()
                && bytes[0..4] == *b"RRaA"
                && bytes[484..488] == *b"rrAa"
                && bytes[508..512] == [0, 0, 0x55, 0xAA];
            structures.push(Structure {
                name: "FSInfo sector".into(),
                role: StructureRole::Other,
                location: Extent::new(at, sector),
                copy: 0,
                integrity: if good {
                    Integrity::Verified
                } else {
                    Integrity::Failed
                },
            });
        }
        let adapter = FatAdapter::new(boot.clone(), self.boot_sector_offset());
        for copy in 0..boot.fat_count {
            structures.push(Structure {
                name: format!("FAT {}", copy + 1),
                role: StructureRole::AllocationMap,
                location: adapter.fat_extent(copy),
                copy: u32::from(copy),
                integrity: adapter.fat_head_integrity(self.device(), copy),
            });
        }
        structures.push(Structure {
            name: "root directory".into(),
            role: StructureRole::Directory,
            location: adapter.root_extent(),
            copy: 0,
            integrity: Integrity::Unchecked,
        });
        structures
    }
}

/// Which salvage features this driver lacks: nothing is stored encoded, and
/// a damaged FAT12 or FAT16 boot sector has no backup to build geometry
/// from, so there is no hypothesis to offer.
pub fn salvage_capabilities() -> Capabilities {
    adapter::capabilities()
}
