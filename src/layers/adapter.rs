//! The FAT salvage adapter: `SalvageAdapter` over the boot sector's geometry.
//!
//! The adapter holds geometry and no device. Every verb decodes from the
//! source it is given, tolerates what a damaged volume holds (a broken chain,
//! an unreadable sector, a directory nobody points to) and reports it.
//!
//! What counts as a candidate: every 32-byte directory entry of every
//! directory cluster, live or deleted. A directory cluster is one that the
//! root's tree reaches, or one whose first entries are `.` and `..` naming
//! itself, found by looking at the head of each cluster in the scanned range.
//! The second kind is how a directory whose parent was lost is found again.
//! The tree's directories are followed through the FAT, cluster by cluster.
//! A cluster the tree does not reach is judged on its own bytes, never by a
//! chain through other clusters, so a scan returns the same records however
//! its range is cut, and resuming it loses nothing. It is a directory
//! cluster when it opens with `.` and `..` naming itself, or when its content
//! is a run of directory entries (`FoundBy::Content`): every 32-byte slot is
//! empty, a long-name entry (sequence 1 to 20, reserved bytes zero) or a
//! short entry (name characters allowed, attribute bits a file can have,
//! first cluster 0 or inside the volume, size fitting the volume and 0 for a
//! directory, real dates where set), and there are at least two short
//! entries, or one whose long-name entries carry its checksum, or, as the
//! weakest evidence (`FatDetail::lone_entry`), one short entry with every
//! other slot empty. A cluster of file data that resembles one entry among
//! other bytes, or a few entries among other bytes, is not accepted. Entries found by content are `Integrity::Unchecked`,
//! below the `Verified` of the tree's, and are reported as live when the
//! cluster is allocated and deleted when it is free in the FAT. A
//! directory's later clusters, once nothing points to it, are found the same
//! way, each on its own content.

use super::cluster_layout;
use crate::boot::{BootSector, FatKind};
use crate::directory::{plausible_short_name, scan_records, short_checksum};
use crate::primitives::{classify_fat_value, FatValue};
use crate::volume::backup_boot;
use fs_core::layers::{
    Candidate, CandidateKind, CandidateState, Capabilities, ClusterLayout, Damage, DamageKind,
    DamageReport, Extent, ExtentWalk, Feature, Integrity, Parent, ParentClaim, SalvageAdapter,
    SalvageError, ScanReport, Severity, Sink, Tree, TreeNode, WalkEnd,
};
use fs_core::BlockRead;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::ControlFlow;

/// The longest run of long-name entries that can precede a short entry.
const LONG_NAME_BYTES: u64 = 20 * 32;

pub(super) fn capabilities() -> Capabilities {
    Capabilities {
        missing: vec![Feature::EncodedExtents, Feature::GeometryHypotheses],
    }
}

/// How the directory cluster that holds an entry was recognised: the
/// evidence behind a candidate, from the strongest to the weakest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FoundBy {
    /// The tree from the root reaches the cluster, by directory entries and
    /// the FAT.
    #[default]
    Tree,
    /// Nothing points to the cluster, but it opens with the `.` and `..`
    /// entries of a directory that names itself.
    DotEntries,
    /// Nothing points to the cluster and it has no `.` and `..`, but its
    /// content is a run of directory entries: see [`FatAdapter`]. The
    /// candidates it yields have `Integrity::Unchecked`.
    Content,
}

/// What a FAT directory entry says, as salvage evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FatDetail {
    /// The 8.3 name, code page 437 decoded, with the NT lower-case flags
    /// applied. A deleted entry's first character is `_`: FAT overwrites it.
    pub short_name: String,
    /// `None` when no long-name entries precede this one (or it is deleted),
    /// `Some(true)` when they form a name that carries its short name's
    /// checksum, `Some(false)` when some precede it and none was usable, so
    /// the long name was not used.
    pub long_name_ok: Option<bool>,
    /// The attribute byte.
    pub attributes: u8,
    /// The first cluster the entry names; 0 for an empty file.
    pub first_cluster: u32,
    /// The size field; 0 for a directory.
    pub size: u32,
    /// The first cluster of the directory that holds the entry; 0 for the
    /// fixed root directory of FAT12 and FAT16.
    pub directory: u32,
    /// How the directory cluster that holds the entry was recognised.
    #[serde(default)]
    pub found_by: FoundBy,
    /// The entry is the only one in its cluster, which nothing points to and
    /// which has no `.` and `..`: the weakest evidence a candidate has, a
    /// single plausible entry with every other slot empty. Only with
    /// `FoundBy::Content`.
    #[serde(default)]
    pub lone_entry: bool,
    /// Creation, modification and access times as `YYYY-MM-DDTHH:MM:SS`,
    /// with no zone, as the entry stores them.
    pub created: Option<String>,
    pub modified: Option<String>,
    pub accessed: Option<String>,
}

/// The geometry the salvage verbs work from. Built from a boot sector that
/// decoded: the primary, or the FAT32 backup when the primary did not.
#[derive(Clone, Debug)]
pub struct FatAdapter {
    boot: BootSector,
    boot_at: u64,
}

impl FatAdapter {
    pub fn new(boot: BootSector, boot_at: u64) -> Self {
        Self { boot, boot_at }
    }

    /// Builds the adapter from a source, reading the primary boot sector,
    /// or the FAT32 backup when the primary does not decode. Fails with
    /// [`SalvageError::NoGeometry`] when neither does.
    pub fn from_source(source: &dyn BlockRead) -> Result<Self, SalvageError> {
        if source.size_bytes() < 512 {
            return Err(SalvageError::NoGeometry);
        }
        let mut sector = [0u8; 512];
        let primary = source
            .read_at(0, &mut sector)
            .ok()
            .and_then(|()| BootSector::parse(&sector).ok());
        match primary {
            Some(boot) => Ok(Self::new(boot, 0)),
            None => backup_boot(source, &sector)
                .map(|(boot, at)| Self::new(boot, at))
                .ok_or(SalvageError::NoGeometry),
        }
    }

    pub fn boot(&self) -> &BootSector {
        &self.boot
    }

    pub(super) fn sector(&self) -> u64 {
        u64::from(self.boot.bytes_per_sector)
    }

    pub(super) fn cluster_size(&self) -> u64 {
        self.boot.cluster_size()
    }

    pub(super) fn layout(&self) -> ClusterLayout {
        cluster_layout(&self.boot)
    }

    pub(super) fn valid_cluster(&self, cluster: u32) -> bool {
        cluster >= 2 && cluster < self.boot.cluster_count + 2
    }

    pub(super) fn cluster_extent(&self, cluster: u32) -> Extent {
        let layout = self.layout();
        Extent::new(
            layout.offset + u64::from(cluster - 2) * layout.size,
            layout.size,
        )
    }

    /// Where the root directory is: the fixed area, or the root cluster.
    pub(super) fn root_extent(&self) -> Extent {
        if self.boot.kind == FatKind::Fat32 {
            self.cluster_extent(self.boot.root_cluster)
        } else {
            self.fixed_root()
        }
    }

    /// The fixed root directory area of FAT12 and FAT16.
    pub(super) fn fixed_root(&self) -> Extent {
        let start = u64::from(self.boot.reserved_sectors)
            + u64::from(self.boot.fat_count) * u64::from(self.boot.sectors_per_fat);
        Extent::new(
            start * self.sector(),
            u64::from(self.boot.root_dir_sectors) * self.sector(),
        )
    }

    pub(super) fn root_first(&self) -> u32 {
        if self.boot.kind == FatKind::Fat32 {
            self.boot.root_cluster
        } else {
            0
        }
    }

    pub(super) fn fat_extent(&self, copy: u8) -> Extent {
        let sectors = u64::from(self.boot.sectors_per_fat);
        Extent::new(
            (u64::from(self.boot.reserved_sectors) + u64::from(copy) * sectors) * self.sector(),
            sectors * self.sector(),
        )
    }

    /// The width of one FAT entry's bytes, and where entry `cluster` starts
    /// inside a FAT.
    fn entry_at(&self, cluster: u32) -> (u64, u64) {
        match self.boot.kind {
            FatKind::Fat12 => (u64::from(cluster) * 3 / 2, 2),
            FatKind::Fat16 => (u64::from(cluster) * 2, 2),
            FatKind::Fat32 => (u64::from(cluster) * 4, 4),
        }
    }

    pub(super) fn decode_entry(&self, cluster: u32, bytes: &[u8]) -> u32 {
        match self.boot.kind {
            FatKind::Fat12 => {
                let word = u16::from_le_bytes([bytes[0], bytes[1]]);
                if cluster & 1 == 0 {
                    u32::from(word & 0x0FFF)
                } else {
                    u32::from(word >> 4)
                }
            }
            FatKind::Fat16 => u32::from(u16::from_le_bytes([bytes[0], bytes[1]])),
            FatKind::Fat32 => u32::from_le_bytes(bytes[..4].try_into().unwrap()) & 0x0FFF_FFFF,
        }
    }

    /// The entry of `cluster` in FAT copy `copy`, classified; the extent
    /// that could not be read when the read failed.
    pub(super) fn fat_value_in(
        &self,
        source: &dyn BlockRead,
        copy: u8,
        cluster: u32,
    ) -> Result<FatValue, Extent> {
        let (at, width) = self.entry_at(cluster);
        let fat = self.fat_extent(copy);
        if at + width > fat.length {
            return Err(Extent::new(fat.offset, fat.length.max(1)));
        }
        let extent = Extent::new(fat.offset + at, width);
        let mut bytes = [0u8; 4];
        source
            .read_at(extent.offset, &mut bytes[..width as usize])
            .map_err(|_| extent)?;
        Ok(classify_fat_value(
            self.boot.kind,
            self.decode_entry(cluster, &bytes),
            self.boot.cluster_count + 1,
        ))
    }

    /// The entry of `cluster`, read from the active FAT and, where that read
    /// fails, from each other copy in turn.
    pub(super) fn fat_value(
        &self,
        source: &dyn BlockRead,
        cluster: u32,
    ) -> Result<FatValue, Extent> {
        let copies = std::iter::once(self.boot.active_fat)
            .chain((0..self.boot.fat_count).filter(|copy| *copy != self.boot.active_fat));
        let mut failure = None;
        for copy in copies {
            match self.fat_value_in(source, copy, cluster) {
                Ok(value) => return Ok(value),
                Err(extent) => failure = failure.or(Some(extent)),
            }
        }
        Err(failure.expect("a volume has at least one FAT"))
    }

    /// Reads `extent`, falling back to one sector at a time when the whole
    /// read fails: the readable sectors, zero-filled where the rest should
    /// be, and the sectors that could not be read.
    pub(super) fn read_lenient(
        &self,
        source: &dyn BlockRead,
        extent: Extent,
    ) -> (Vec<u8>, Vec<Extent>) {
        let mut bytes = vec![0u8; extent.length as usize];
        if source.read_at(extent.offset, &mut bytes).is_ok() {
            return (bytes, Vec::new());
        }
        let step = self.sector();
        let mut unreadable: Vec<Extent> = Vec::new();
        let mut at = 0u64;
        while at < extent.length {
            let length = step.min(extent.length - at);
            let part = &mut bytes[at as usize..(at + length) as usize];
            if source.read_at(extent.offset + at, part).is_err() {
                part.fill(0);
                match unreadable.last_mut() {
                    Some(last) if last.end() == Some(extent.offset + at) => last.length += length,
                    _ => unreadable.push(Extent::new(extent.offset + at, length)),
                }
            }
            at += length;
        }
        (bytes, unreadable)
    }

    /// The chain from `first` by the FAT, as far as it goes: stops at its
    /// end, at an entry that is not a next cluster, on a loop, or where the
    /// FAT cannot be read (that extent is returned with it).
    pub(super) fn follow(&self, source: &dyn BlockRead, first: u32) -> (Vec<u32>, Option<Extent>) {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cluster = first;
        while self.valid_cluster(cluster) && seen.insert(cluster) {
            chain.push(cluster);
            match self.fat_value(source, cluster) {
                Ok(FatValue::Next(next)) => cluster = next,
                Ok(_) => break,
                Err(extent) => return (chain, Some(extent)),
            }
        }
        (chain, None)
    }

    fn first_fat_bytes(&self, source: &dyn BlockRead, copy: u8, length: u64) -> Option<Vec<u8>> {
        let fat = self.fat_extent(copy);
        let mut bytes = vec![0u8; length.min(fat.length) as usize];
        source.read_at(fat.offset, &mut bytes).ok()?;
        Some(bytes)
    }

    /// Whether a FAT's first entries hold what the boot sector's media
    /// descriptor and the format say they must, and match the first copy.
    pub(super) fn fat_head_integrity(&self, source: &dyn BlockRead, copy: u8) -> Integrity {
        let Some(head) = self.first_fat_bytes(source, copy, 4096) else {
            return Integrity::Failed;
        };
        let Some(media) = self.media_byte(source) else {
            return Integrity::Unchecked;
        };
        let mut good = head.len() >= 4 && head[0] == media;
        if copy != 0 {
            good &= self.first_fat_bytes(source, 0, 4096).as_deref() == Some(&head[..]);
        }
        if good {
            Integrity::Verified
        } else {
            Integrity::Failed
        }
    }

    pub(super) fn media_byte(&self, source: &dyn BlockRead) -> Option<u8> {
        let mut byte = [0u8; 1];
        source.read_at(self.boot_at + 21, &mut byte).ok()?;
        Some(byte[0])
    }

    /// The quick verdict's findings: the boot sectors, a volume longer than
    /// its source, the heads of the FAT copies and the FAT's own flags.
    pub(super) fn health_findings(&self, source: &dyn BlockRead, volume_len: u64) -> Vec<Damage> {
        let mut findings = self.boot_findings(source);
        let declared = u64::from(self.boot.total_sectors) * self.sector();
        if volume_len < declared {
            findings.push(Damage {
                kind: DamageKind::Missing,
                severity: Severity::Error,
                location: Extent::new(volume_len, declared - volume_len),
                structure: "volume".into(),
                detail: format!(
                    "the boot sector declares {declared} bytes and the source holds {volume_len}"
                ),
            });
        }
        findings.extend(self.fat_head_findings(source));
        findings
    }

    pub(super) fn fat_head_findings(&self, source: &dyn BlockRead) -> Vec<Damage> {
        let mut findings = Vec::new();
        let fat_bytes = self.fat_extent(0).length;
        let head_length = fat_bytes.min(32 * 1024);
        let reference = self.first_fat_bytes(source, 0, head_length);
        if reference.is_none() {
            findings.push(Damage {
                kind: DamageKind::Unreadable,
                severity: Severity::Error,
                location: self.fat_extent(0),
                structure: "FAT 1".into(),
                detail: "the start of the FAT cannot be read".into(),
            });
        }
        if let (Some(reference), Some(media)) = (&reference, self.media_byte(source)) {
            if reference[0] != media {
                findings.push(Damage {
                    kind: DamageKind::Mismatch,
                    severity: Severity::Warning,
                    location: Extent::new(self.fat_extent(0).offset, 1),
                    structure: "FAT 1".into(),
                    detail: format!(
                        "the first FAT entry starts with 0x{:02X} and the media descriptor is 0x{media:02X}",
                        reference[0]
                    ),
                });
            }
        }
        if let Some(reference) = &reference {
            for copy in 1..self.boot.fat_count {
                let fat = self.fat_extent(copy);
                match self.first_fat_bytes(source, copy, head_length) {
                    None => findings.push(Damage {
                        kind: DamageKind::Unreadable,
                        severity: Severity::Warning,
                        location: fat,
                        structure: format!("FAT {}", copy + 1),
                        detail: "the start of this FAT copy cannot be read".into(),
                    }),
                    Some(bytes) if bytes != *reference => findings.push(Damage {
                        kind: DamageKind::Mismatch,
                        severity: Severity::Warning,
                        location: fat,
                        structure: format!("FAT {}", copy + 1),
                        detail: "the start of this FAT copy differs from the first".into(),
                    }),
                    Some(_) => {}
                }
            }
            if let Some(flags) = self.volume_flags(reference) {
                findings.extend(flags);
            }
        }
        findings
    }

    /// The findings `volume_flags` makes, none for FAT12.
    pub(super) fn volume_flag_findings(&self, head: &[u8]) -> Vec<Damage> {
        self.volume_flags(head).unwrap_or_default()
    }

    /// What FAT entry 1 of a FAT16 or FAT32 volume records: whether it was
    /// unmounted cleanly and whether the driver met a disk error.
    fn volume_flags(&self, head: &[u8]) -> Option<Vec<Damage>> {
        let (clean, no_error) = match self.boot.kind {
            FatKind::Fat12 => return None,
            FatKind::Fat16 => {
                let word = u16::from_le_bytes(head.get(2..4)?.try_into().ok()?);
                (word & 0x8000 != 0, word & 0x4000 != 0)
            }
            FatKind::Fat32 => {
                let word = u32::from_le_bytes(head.get(4..8)?.try_into().ok()?);
                (word & 0x0800_0000 != 0, word & 0x0400_0000 != 0)
            }
        };
        let location = Extent::new(
            self.fat_extent(0).offset + self.entry_at(1).0,
            self.entry_at(1).1,
        );
        let mut findings = Vec::new();
        if !clean {
            findings.push(Damage {
                kind: DamageKind::Recorded,
                severity: Severity::Notice,
                location,
                structure: "FAT 1".into(),
                detail: "the volume was not unmounted cleanly".into(),
            });
        }
        if !no_error {
            findings.push(Damage {
                kind: DamageKind::Recorded,
                severity: Severity::Warning,
                location,
                structure: "FAT 1".into(),
                detail: "the volume records a disk error".into(),
            });
        }
        Some(findings)
    }

    /// The boot sector copies and FSInfo, as findings.
    pub(super) fn boot_findings(&self, source: &dyn BlockRead) -> Vec<Damage> {
        let sector = self.sector();
        let mut findings = Vec::new();
        let read = |offset: u64| {
            let mut bytes = [0u8; 512];
            source.read_at(offset, &mut bytes).ok().map(|()| bytes)
        };
        let mut primary_raw = None;
        match read(0) {
            None => findings.push(Damage {
                kind: DamageKind::Unreadable,
                severity: Severity::Error,
                location: Extent::new(0, sector),
                structure: "boot sector".into(),
                detail: "the boot sector cannot be read".into(),
            }),
            Some(bytes) => match BootSector::parse(&bytes) {
                Ok(parsed) => {
                    primary_raw = Some(bytes);
                    if !parsed.signature_ok {
                        findings.push(Damage {
                            kind: DamageKind::Signature,
                            severity: Severity::Warning,
                            location: Extent::new(510, 2),
                            structure: "boot sector".into(),
                            detail: "the 0x55 0xAA signature is missing; the BPB is intact".into(),
                        });
                    }
                }
                Err(error) => findings.push(Damage {
                    kind: if bytes[510..512] == [0x55, 0xAA] {
                        DamageKind::Malformed
                    } else {
                        DamageKind::Signature
                    },
                    severity: Severity::Error,
                    location: Extent::new(0, sector),
                    structure: "boot sector".into(),
                    detail: format!("the boot sector does not decode: {error}"),
                }),
            },
        }
        if self.boot.kind != FatKind::Fat32 {
            return findings;
        }
        let used = read(self.boot_at);
        let field = |at: usize| {
            used.as_ref().and_then(|bytes| {
                let sector = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
                (sector != 0 && sector != 0xFFFF).then_some(u64::from(sector))
            })
        };
        if let Some(backup) = field(50) {
            let at = backup * sector;
            match read(at) {
                None => findings.push(Damage {
                    kind: DamageKind::Unreadable,
                    severity: Severity::Warning,
                    location: Extent::new(at, sector),
                    structure: "backup boot sector".into(),
                    detail: "the backup boot sector cannot be read".into(),
                }),
                Some(bytes) => match BootSector::parse(&bytes) {
                    Err(error) => findings.push(Damage {
                        kind: DamageKind::Malformed,
                        severity: Severity::Warning,
                        location: Extent::new(at, sector),
                        structure: "backup boot sector".into(),
                        detail: format!("the backup boot sector does not decode: {error}"),
                    }),
                    Ok(_) => {
                        if primary_raw.is_some_and(|primary| primary != bytes) {
                            findings.push(Damage {
                                kind: DamageKind::Mismatch,
                                severity: Severity::Warning,
                                location: Extent::new(at, sector),
                                structure: "backup boot sector".into(),
                                detail: "the backup boot sector differs from the primary".into(),
                            });
                        }
                    }
                },
            }
        }
        if let Some(info) = field(48) {
            let at = info * sector;
            let good = read(at).is_some_and(|bytes| {
                bytes[0..4] == *b"RRaA"
                    && bytes[484..488] == *b"rrAa"
                    && bytes[508..512] == [0, 0, 0x55, 0xAA]
            });
            if !good {
                findings.push(Damage {
                    kind: DamageKind::Signature,
                    severity: Severity::Warning,
                    location: Extent::new(at, sector),
                    structure: "FSInfo sector".into(),
                    detail: "the FSInfo sector's signatures are wrong or it cannot be read".into(),
                });
            }
        }
        findings
    }
}

/// A directory cluster and what the walk learned about it.
#[derive(Clone, Copy, Debug)]
pub(super) struct DirCluster {
    /// The first cluster of the directory the cluster belongs to.
    pub directory: u32,
    /// The cluster before it in that directory's chain.
    pub previous: Option<u32>,
    /// The directory was freed: its first cluster is free in the FAT.
    pub deleted: bool,
    pub found_by: FoundBy,
    pub lone: bool,
}

/// The directory clusters of a volume.
#[derive(Default)]
pub(super) struct Discovery {
    pub clusters: BTreeMap<u32, DirCluster>,
    /// Sectors of the FAT that could not be read while following chains.
    pub unreadable_fat: Vec<Extent>,
}

impl FatAdapter {
    /// Finds the directory clusters: the root's tree, then, within
    /// `heads`, clusters that open with `.` and `..` naming themselves.
    pub(super) fn discover(&self, source: &dyn BlockRead, heads: Option<Extent>) -> Discovery {
        let mut found = Discovery::default();
        let mut queue: VecDeque<(u32, bool)> = VecDeque::new();
        if self.boot.kind == FatKind::Fat32 {
            queue.push_back((self.boot.root_cluster, false));
        } else {
            let (bytes, _) = self.read_lenient(source, self.fixed_root());
            self.queue_subdirectories(&bytes, false, &mut queue);
        }
        self.drain(source, &mut queue, &mut found);
        if let Some(range) = heads {
            // A directory the tree does not reach is known by its first
            // cluster alone: its later clusters could only be found by
            // following the FAT from a head that a narrower range may not
            // hold, and the same scan must give the same records however
            // its range is cut.
            for (cluster, found_by, lone) in self.head_candidates(source, range, &found) {
                let freed = matches!(self.fat_value(source, cluster), Ok(FatValue::Free));
                found.clusters.insert(
                    cluster,
                    DirCluster {
                        directory: cluster,
                        previous: None,
                        deleted: freed,
                        found_by,
                        lone,
                    },
                );
            }
        }
        found
    }

    fn drain(
        &self,
        source: &dyn BlockRead,
        queue: &mut VecDeque<(u32, bool)>,
        found: &mut Discovery,
    ) {
        while let Some((first, inherited_deleted)) = queue.pop_front() {
            if !self.valid_cluster(first) || found.clusters.contains_key(&first) {
                continue;
            }
            let (chain, failure) = self.follow(source, first);
            if let Some(extent) = failure {
                found.unreadable_fat.push(extent);
            }
            let freed = matches!(self.fat_value(source, first), Ok(FatValue::Free));
            let deleted = inherited_deleted || freed;
            // A freed directory's chain is gone: only its first cluster is known.
            let chain = if freed { vec![first] } else { chain };
            let mut previous = None;
            for cluster in chain {
                if found.clusters.contains_key(&cluster) {
                    break;
                }
                found.clusters.insert(
                    cluster,
                    DirCluster {
                        directory: first,
                        previous,
                        deleted,
                        found_by: FoundBy::Tree,
                        lone: false,
                    },
                );
                previous = Some(cluster);
                let (bytes, _) = self.read_lenient(source, self.cluster_extent(cluster));
                self.queue_subdirectories(&bytes, deleted, queue);
            }
        }
    }

    fn queue_subdirectories(&self, bytes: &[u8], deleted: bool, queue: &mut VecDeque<(u32, bool)>) {
        let Ok(records) = scan_records(bytes, self.boot.kind, true) else {
            return;
        };
        for scanned in records {
            let entry = scanned.record.entry;
            if !entry.deleted
                && entry.attributes & 0x10 != 0
                && entry.name != "."
                && entry.name != ".."
                && self.valid_cluster(entry.first_cluster)
            {
                queue.push_back((entry.first_cluster, deleted));
            }
        }
    }

    /// Clusters in `range`, not yet known as directories, that open with the
    /// `.` and `..` entries of a directory that names itself.
    fn head_candidates(
        &self,
        source: &dyn BlockRead,
        range: Extent,
        found: &Discovery,
    ) -> Vec<(u32, FoundBy, bool)> {
        let layout = self.layout();
        let Some(end) = range.end() else {
            return Vec::new();
        };
        let first = range.offset.max(layout.offset);
        let last = end.min(layout.offset + layout.count * layout.size);
        if last <= first {
            return Vec::new();
        }
        let from = (first - layout.offset) / layout.size;
        let to = (last - layout.offset).div_ceil(layout.size);
        let mut heads = Vec::new();
        let mut bytes = [0u8; 64];
        for index in from..to {
            let cluster = 2 + index as u32;
            if found.clusters.contains_key(&cluster) {
                continue;
            }
            let extent = self.cluster_extent(cluster);
            if source.read_at(extent.offset, &mut bytes).is_err() {
                continue;
            }
            if self.opens_a_directory(&bytes, cluster) {
                heads.push((cluster, FoundBy::DotEntries, false));
                continue;
            }
            // Only a cluster that starts with an entry is read whole.
            if !self.plausible_slot(&bytes[..32])
                || extent.end().is_none_or(|e| e > source.size_bytes())
            {
                continue;
            }
            let (whole, unreadable) = self.read_lenient(source, extent);
            if unreadable.is_empty() {
                if let Some(lone) = self.looks_like_directory(&whole) {
                    heads.push((cluster, FoundBy::Content, lone));
                }
            }
        }
        heads
    }

    /// Whether a 32-byte slot is a long-name entry or a short entry that could
    /// be a directory's: a name of allowed characters, attribute bits a file
    /// can have, a first cluster that is 0 or inside the volume, a size that
    /// fits the volume (0 for a directory, and for no cluster), and dates
    /// that, where set, are real. A zero slot is not one.
    fn plausible_slot(&self, raw: &[u8]) -> bool {
        if raw.iter().all(|byte| *byte == 0) {
            return false;
        }
        if raw[11] == 0x0F {
            let sequence = raw[0] & 0x1F;
            return (1..=20).contains(&sequence) && raw[12] == 0 && raw[26] == 0 && raw[27] == 0;
        }
        let word = |at: usize| u16::from_le_bytes([raw[at], raw[at + 1]]);
        let high = if self.boot.kind == FatKind::Fat32 {
            u32::from(word(20)) & 0x0FFF
        } else {
            0
        };
        let first = (high << 16) | u32::from(word(26));
        let size = u32::from_le_bytes(raw[28..32].try_into().unwrap());
        let date_ok = |date: u16| {
            date == 0 || ((1..=12).contains(&((date >> 5) & 15)) && (1..=31).contains(&(date & 31)))
        };
        let size_ok = if raw[11] & 0x10 != 0 {
            size == 0
        } else {
            (first != 0 || size == 0)
                && u64::from(size) <= u64::from(self.boot.cluster_count) * self.cluster_size()
        };
        plausible_short_name(raw)
            && (first == 0 || self.valid_cluster(first))
            && size_ok
            && date_ok(word(16))
            && date_ok(word(24))
    }

    /// Whether a cluster's content is a directory's. Every slot is empty or
    /// plausible, and there is a run: two short entries, or one whose
    /// long-name entries carry its checksum. File data that resembles one
    /// entry, or a few entries among bytes that are not, is not accepted.
    ///
    /// `Some(true)` is the weakest case: one plausible short entry, no
    /// long-name entries, every other slot empty. Anything else non-empty
    /// beside a single entry refuses the cluster.
    fn looks_like_directory(&self, cluster: &[u8]) -> Option<bool> {
        let mut shorts = 0;
        let mut longs = 0;
        let mut verified = 0;
        let mut checksum = None;
        for raw in cluster.chunks_exact(32) {
            if raw.iter().all(|byte| *byte == 0) {
                checksum = None;
                continue;
            }
            if !self.plausible_slot(raw) {
                return None;
            }
            if raw[11] == 0x0F {
                longs += 1;
                checksum = Some(raw[13]);
            } else {
                shorts += 1;
                if checksum == Some(short_checksum(&raw[..11])) {
                    verified += 1;
                }
                checksum = None;
            }
        }
        if shorts >= 2 || verified >= 1 {
            Some(false)
        } else {
            (shorts == 1 && longs == 0).then_some(true)
        }
    }

    fn opens_a_directory(&self, head: &[u8; 64], cluster: u32) -> bool {
        let dot = &head[..32];
        let dotdot = &head[32..];
        let first_cluster = |entry: &[u8]| {
            let high = if self.boot.kind == FatKind::Fat32 {
                u32::from(u16::from_le_bytes([entry[20], entry[21]])) & 0x0FFF
            } else {
                0
            };
            (high << 16) | u32::from(u16::from_le_bytes([entry[26], entry[27]]))
        };
        dot[..11] == *b".          "
            && dot[11] & 0x10 != 0
            && dot[11] != 0x0F
            && first_cluster(dot) == cluster
            && dotdot[..11] == *b"..         "
            && dotdot[11] & 0x10 != 0
    }
}

/// `YYYY-MM-DDTHH:MM:SS` from FAT date and time fields, or `None` for a
/// field that holds no valid time.
fn time_text(date: u16, time: u16) -> Option<String> {
    let (year, month, day) = (1980 + (date >> 9), (date >> 5) & 15, date & 31);
    let (hour, minute, second) = (time >> 11, (time >> 5) & 63, (time & 31) * 2);
    ((1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour < 24
        && minute < 60
        && second < 60)
        .then(|| format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"))
}

impl SalvageAdapter for FatAdapter {
    type Detail = FatDetail;

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    fn find_candidates(
        &self,
        source: &dyn BlockRead,
        range: Extent,
        sink: &mut dyn Sink<Candidate<FatDetail>>,
    ) -> Result<ScanReport, SalvageError> {
        let size = source.size_bytes();
        let end = range
            .end()
            .filter(|end| *end <= size)
            .ok_or(SalvageError::Bounds)?;
        let mut report = ScanReport::default();
        if range.length == 0 {
            return Ok(report);
        }
        let discovery = self.discover(source, Some(range));
        let clip = |extent: Extent| -> Option<Extent> {
            let from = extent.offset.max(range.offset);
            let to = extent.end()?.min(end);
            (to > from).then(|| Extent::new(from, to - from))
        };
        let mut areas: Vec<(Extent, u32, Option<u32>, bool, FoundBy, bool)> = Vec::new();
        if self.boot.kind != FatKind::Fat32 {
            areas.push((self.fixed_root(), 0, None, false, FoundBy::Tree, false));
        }
        for (cluster, info) in &discovery.clusters {
            areas.push((
                self.cluster_extent(*cluster),
                info.directory,
                info.previous,
                info.deleted,
                info.found_by,
                info.lone,
            ));
        }
        for (area, directory, previous, deleted, found_by, lone) in areas {
            let Some(area_end) = area.end() else { continue };
            if area.offset >= end || area_end <= range.offset || area_end > size {
                continue;
            }
            let (bytes, unreadable) = self.read_lenient(source, area);
            report
                .unreadable
                .extend(unreadable.into_iter().filter_map(clip));
            let (prefix, _) = match previous {
                Some(previous) => {
                    let extent = self.cluster_extent(previous);
                    let length = LONG_NAME_BYTES.min(extent.length);
                    self.read_lenient(
                        source,
                        Extent::new(extent.offset + extent.length - length, length),
                    )
                }
                None => (Vec::new(), Vec::new()),
            };
            let skip = prefix.len();
            let mut joined = prefix;
            joined.extend_from_slice(&bytes);
            let records = scan_records(&joined, self.boot.kind, true)
                .map_err(|_| SalvageError::NoGeometry)?;
            for scanned in records {
                if scanned.record.offset < skip {
                    continue;
                }
                let at = area.offset + (scanned.record.offset - skip) as u64;
                if at < range.offset || at >= end {
                    continue;
                }
                let entry = &scanned.record.entry;
                if entry.name == "." || entry.name == ".." {
                    continue;
                }
                let raw = &scanned.record.raw;
                let word = |from: usize| u16::from_le_bytes([raw[from], raw[from + 1]]);
                let kind = if entry.attributes & 0x10 != 0 {
                    CandidateKind::Directory
                } else {
                    CandidateKind::File
                };
                let in_range = self.valid_cluster(entry.first_cluster);
                let integrity = if scanned.long_name_ok == Some(false)
                    || (!in_range
                        && !entry.deleted
                        && (entry.size > 0 || kind == CandidateKind::Directory))
                {
                    Integrity::Failed
                } else if found_by == FoundBy::Content {
                    Integrity::Unchecked
                } else {
                    Integrity::Verified
                };
                let candidate = Candidate {
                    location: Extent::new(at, 32),
                    kind,
                    state: if entry.deleted || deleted {
                        CandidateState::Deleted
                    } else {
                        CandidateState::Live
                    },
                    integrity,
                    name: Some(entry.name.clone()),
                    size: (kind == CandidateKind::File).then_some(u64::from(entry.size)),
                    detail: FatDetail {
                        short_name: crate::directory::short_name(raw, entry.deleted),
                        long_name_ok: scanned.long_name_ok,
                        attributes: entry.attributes,
                        first_cluster: entry.first_cluster,
                        size: entry.size,
                        directory,
                        found_by,
                        lone_entry: lone,
                        created: time_text(word(16), word(14)),
                        modified: time_text(word(24), word(22)),
                        accessed: time_text(word(18), 0),
                    },
                };
                report.candidates += 1;
                if let ControlFlow::Break(()) = sink.accept(candidate) {
                    report.stopped_at = Some(at + 32);
                    report.unreadable.sort();
                    report.unreadable.dedup();
                    return Ok(report);
                }
            }
        }
        report.unreadable.sort();
        report.unreadable.dedup();
        Ok(report)
    }

    fn walk_extents(
        &self,
        source: &dyn BlockRead,
        candidate: &Candidate<FatDetail>,
    ) -> Result<ExtentWalk, SalvageError> {
        let detail = &candidate.detail;
        let cluster_size = self.cluster_size();
        let directory = candidate.kind == CandidateKind::Directory;
        let claimed_size = if directory { 0 } else { u64::from(detail.size) };
        let needed = (!directory).then(|| claimed_size.div_ceil(cluster_size));
        if needed == Some(0) {
            return Ok(ExtentWalk::contiguous(Vec::new(), 0, WalkEnd::Complete));
        }
        let size = source.size_bytes();
        let deleted = candidate.state == CandidateState::Deleted;
        let mut runs: Vec<Extent> = Vec::new();
        let mut count = 0u64;
        let mut seen = HashSet::new();
        let mut cluster = detail.first_cluster;
        let end = loop {
            if !self.valid_cluster(cluster) {
                break WalkEnd::OutOfRange;
            }
            let extent = self.cluster_extent(cluster);
            if extent.end().is_none_or(|end| end > size) {
                break WalkEnd::OutOfRange;
            }
            if !seen.insert(cluster) {
                break WalkEnd::Cycle;
            }
            let value = self.fat_value(source, cluster);
            if deleted && !matches!(value, Ok(FatValue::Free)) {
                break match value {
                    Err(unreadable) => WalkEnd::Unreadable(unreadable),
                    Ok(_) => WalkEnd::Broken,
                };
            }
            match runs.last_mut() {
                Some(last) if last.end() == Some(extent.offset) => last.length += extent.length,
                _ => runs.push(extent),
            }
            count += 1;
            let wanted = needed.unwrap_or(1);
            if deleted {
                // A freed chain is gone from the FAT: the file is taken to
                // have been stored in one run, as a formatter leaves it.
                if count == wanted {
                    break WalkEnd::Complete;
                }
                cluster += 1;
                continue;
            }
            if Some(count) == needed {
                break WalkEnd::Complete;
            }
            match value {
                Ok(FatValue::Next(next)) => cluster = next,
                Ok(FatValue::EndOfChain) if directory => break WalkEnd::Complete,
                Ok(_) => break WalkEnd::Broken,
                Err(unreadable) => break WalkEnd::Unreadable(unreadable),
            }
        };
        let mapped = count * cluster_size;
        let claimed = if directory { mapped } else { claimed_size };
        if mapped > claimed {
            if let Some(last) = runs.last_mut() {
                last.length -= mapped - claimed;
            }
        }
        Ok(ExtentWalk::contiguous(runs, claimed, end))
    }

    fn rebuild_tree(
        &self,
        _source: &dyn BlockRead,
        candidates: &[Candidate<FatDetail>],
    ) -> Result<Tree, SalvageError> {
        let mut directories: HashMap<u32, Vec<u64>> = HashMap::new();
        for candidate in candidates {
            if candidate.kind == CandidateKind::Directory
                && self.valid_cluster(candidate.detail.first_cluster)
            {
                directories
                    .entry(candidate.detail.first_cluster)
                    .or_default()
                    .push(candidate.location.offset);
            }
        }
        let root = self.root_first();
        let nodes = candidates
            .iter()
            .map(|candidate| {
                let directory = candidate.detail.directory;
                let mut parents = Vec::new();
                if directory == root {
                    parents.push(Parent::Root);
                } else if let Some(offsets) = directories.get(&directory) {
                    let mut offsets: Vec<u64> = offsets
                        .iter()
                        .copied()
                        .filter(|offset| *offset != candidate.location.offset)
                        .collect();
                    offsets.sort_unstable();
                    parents.extend(offsets.into_iter().map(Parent::Candidate));
                }
                TreeNode {
                    candidate: candidate.location.offset,
                    parents: parents
                        .into_iter()
                        .map(|parent| ParentClaim {
                            parent,
                            name: candidate
                                .name
                                .as_ref()
                                .map(|name| name.clone().into_bytes()),
                            state: candidate.state,
                        })
                        .collect(),
                }
            })
            .collect();
        Ok(Tree { nodes })
    }

    fn report_damage(
        &self,
        source: &dyn BlockRead,
        sink: &mut dyn Sink<Damage>,
    ) -> Result<DamageReport, SalvageError> {
        super::damage::report(self, source, sink)
    }

    fn read_extent(
        &self,
        source: &dyn BlockRead,
        extent: Extent,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, SalvageError> {
        let end = extent
            .end()
            .filter(|end| *end <= source.size_bytes())
            .ok_or(SalvageError::Bounds)?;
        if offset > extent.length {
            return Err(SalvageError::Bounds);
        }
        let count = ((end - extent.offset - offset) as usize).min(output.len());
        source
            .read_at(extent.offset + offset, &mut output[..count])
            .map_err(SalvageError::Device)?;
        Ok(count)
    }

    fn cluster_layout(&self) -> Result<ClusterLayout, SalvageError> {
        Ok(self.layout())
    }
}
