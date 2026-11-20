//! `report_damage` for FAT: the full scan of the volume's own structures.
//!
//! In order: the boot sector copies and FSInfo, a volume longer than its
//! source, the FAT (its copies against each other, every entry, the flags in
//! entry 1), then the namespace from the root: each directory's chain, each
//! entry's start cluster, long-name checksums, and each file's chain against
//! its size, with every cluster claimed once. Unreadable bytes are findings,
//! never errors.

use super::adapter::FatAdapter;
use crate::boot::FatKind;
use crate::directory::scan_records;
use crate::primitives::FatValue;
use fs_core::layers::{Damage, DamageKind, DamageReport, Extent, SalvageError, Severity, Sink};
use fs_core::BlockRead;
use std::collections::{HashSet, VecDeque};
use std::ops::ControlFlow;

/// Clusters whose FAT entries are read, and compared across copies, at once.
const FAT_BLOCK_CLUSTERS: u32 = 16 * 1024;

struct Out<'a> {
    sink: &'a mut dyn Sink<Damage>,
    report: DamageReport,
    errors: bool,
}

impl Out<'_> {
    /// Hands a finding to the sink; false once the sink has stopped.
    fn emit(&mut self, finding: Damage) -> bool {
        if self.report.stopped {
            return false;
        }
        self.errors |= finding.severity == Severity::Error;
        self.report.reported += 1;
        if let ControlFlow::Break(()) = self.sink.accept(finding) {
            self.report.stopped = true;
        }
        !self.report.stopped
    }

    fn unreadable(&mut self, extents: Vec<Extent>, structure: &str, severity: Severity) -> bool {
        for extent in extents {
            let finding = Damage {
                kind: DamageKind::Unreadable,
                severity,
                location: extent,
                structure: structure.to_owned(),
                detail: "these bytes cannot be read".into(),
            };
            if !self.emit(finding) {
                return false;
            }
        }
        true
    }
}

/// One bit per cluster.
struct Bits(Vec<u64>);

impl Bits {
    fn new(count: u32) -> Self {
        Self(vec![0; (count as usize).div_ceil(64) + 1])
    }

    fn get(&self, index: u32) -> bool {
        self.0[index as usize / 64] >> (index % 64) & 1 == 1
    }

    /// Sets a bit and says whether it was already set.
    fn set(&mut self, index: u32) -> bool {
        let word = &mut self.0[index as usize / 64];
        let was = *word >> (index % 64) & 1 == 1;
        *word |= 1 << (index % 64);
        was
    }
}

pub(super) fn report(
    adapter: &FatAdapter,
    source: &dyn BlockRead,
    sink: &mut dyn Sink<Damage>,
) -> Result<DamageReport, SalvageError> {
    let mut out = Out {
        sink,
        report: DamageReport::default(),
        errors: false,
    };
    let boot = adapter.boot();
    for finding in adapter.boot_findings(source) {
        if !out.emit(finding) {
            return Ok(out.report);
        }
    }
    let declared = u64::from(boot.total_sectors) * adapter.sector();
    if source.size_bytes() < declared {
        let finding = Damage {
            kind: DamageKind::Missing,
            severity: Severity::Error,
            location: Extent::new(source.size_bytes(), declared - source.size_bytes()),
            structure: "volume".into(),
            detail: format!(
                "the boot sector declares {declared} bytes and the source holds {}",
                source.size_bytes()
            ),
        };
        if !out.emit(finding) {
            return Ok(out.report);
        }
    }
    let allocated = fat(adapter, source, &mut out);
    if out.report.stopped {
        return Ok(out.report);
    }
    namespace(adapter, source, &allocated, &mut out);
    Ok(out.report)
}

/// The FAT: media descriptor, flags, copies against each other, entries.
/// Returns the clusters it found allocated.
fn fat(adapter: &FatAdapter, source: &dyn BlockRead, out: &mut Out<'_>) -> Bits {
    let boot = adapter.boot();
    let mut allocated = Bits::new(boot.cluster_count + 2);
    let active = boot.active_fat;
    let mut first_block = true;
    let mut cluster = 0u32;
    // A run of blocks where a copy differs from the active FAT, to report once.
    let mut differing: Vec<Option<Extent>> = vec![None; usize::from(boot.fat_count)];
    while cluster < boot.cluster_count + 2 {
        let upto = (cluster + FAT_BLOCK_CLUSTERS).min(boot.cluster_count + 2);
        let (from, _) = entry_bytes(adapter, cluster);
        let (last, width) = entry_bytes(adapter, upto - 1);
        let length = last + width - from;
        let fat = adapter.fat_extent(active);
        if from + length > fat.length {
            break;
        }
        let block = Extent::new(fat.offset + from, length);
        let (bytes, unreadable) = adapter.read_lenient(source, block);
        if !out.unreadable(unreadable, "FAT", Severity::Error) {
            return allocated;
        }
        if first_block {
            first_block = false;
            if let Some(media) = adapter.media_byte(source) {
                if bytes[0] != media {
                    let finding = Damage {
                        kind: DamageKind::Mismatch,
                        severity: Severity::Warning,
                        location: Extent::new(fat.offset, 1),
                        structure: "FAT".into(),
                        detail: format!(
                            "the first FAT entry starts with 0x{:02X} and the media descriptor is 0x{media:02X}",
                            bytes[0]
                        ),
                    };
                    if !out.emit(finding) {
                        return allocated;
                    }
                }
            }
            for finding in adapter.volume_flag_findings(&bytes[..(8.min(bytes.len()))]) {
                if !out.emit(finding) {
                    return allocated;
                }
            }
        }
        for copy in (0..boot.fat_count).filter(|copy| *copy != active) {
            let other = adapter.fat_extent(copy);
            let there = Extent::new(other.offset + from, length);
            let (theirs, unreadable) = adapter.read_lenient(source, there);
            let name = format!("FAT {}", copy + 1);
            if !out.unreadable(unreadable, &name, Severity::Warning) {
                return allocated;
            }
            let slot = &mut differing[usize::from(copy)];
            if theirs != bytes {
                match slot {
                    Some(run) if run.end() == Some(there.offset) => run.length += there.length,
                    Some(run) => {
                        let finding = mismatch(*run, &name);
                        *slot = Some(there);
                        if !out.emit(finding) {
                            return allocated;
                        }
                    }
                    None => *slot = Some(there),
                }
            } else if let Some(run) = slot.take() {
                if !out.emit(mismatch(run, &name)) {
                    return allocated;
                }
            }
        }
        for number in cluster.max(2)..upto {
            let (at, width) = entry_bytes(adapter, number);
            let entry = &bytes[(at - from) as usize..(at - from + width) as usize];
            let raw = adapter.decode_entry(number, entry);
            let value =
                crate::primitives::classify_fat_value(boot.kind, raw, boot.cluster_count + 1);
            let location = Extent::new(fat.offset + at, width);
            let finding = match value {
                FatValue::Free => None,
                FatValue::Bad => Some(Damage {
                    kind: DamageKind::Recorded,
                    severity: Severity::Notice,
                    location: adapter.cluster_extent(number),
                    structure: "FAT".into(),
                    detail: format!("cluster {number} is marked bad"),
                }),
                FatValue::OutOfRange(value) => {
                    allocated.set(number);
                    Some(Damage {
                        kind: DamageKind::Malformed,
                        severity: Severity::Error,
                        location,
                        structure: "FAT".into(),
                        detail: format!("cluster {number} points to {value}, outside the volume"),
                    })
                }
                FatValue::Reserved(value) => {
                    allocated.set(number);
                    Some(Damage {
                        kind: DamageKind::Malformed,
                        severity: Severity::Warning,
                        location,
                        structure: "FAT".into(),
                        detail: format!("cluster {number} holds the reserved value 0x{value:X}"),
                    })
                }
                FatValue::Next(_) | FatValue::EndOfChain => {
                    allocated.set(number);
                    None
                }
            };
            if let Some(finding) = finding {
                if !out.emit(finding) {
                    return allocated;
                }
            }
        }
        cluster = upto;
    }
    for copy in 0..boot.fat_count {
        if let Some(run) = differing[usize::from(copy)].take() {
            if !out.emit(mismatch(run, &format!("FAT {}", copy + 1))) {
                break;
            }
        }
    }
    allocated
}

fn mismatch(extent: Extent, name: &str) -> Damage {
    Damage {
        kind: DamageKind::Mismatch,
        severity: Severity::Warning,
        location: extent,
        structure: name.to_owned(),
        detail: "this part of the FAT copy differs from the active FAT".into(),
    }
}

/// Where entry `cluster` is inside a FAT, and how many bytes it spans.
fn entry_bytes(adapter: &FatAdapter, cluster: u32) -> (u64, u64) {
    match adapter.boot().kind {
        FatKind::Fat12 => (u64::from(cluster) * 3 / 2, 2),
        FatKind::Fat16 => (u64::from(cluster) * 2, 2),
        FatKind::Fat32 => (u64::from(cluster) * 4, 4),
    }
}

/// How a chain ends, judged by the FAT value after its last cluster.
fn chain_fault(adapter: &FatAdapter, source: &dyn BlockRead, chain: &[u32]) -> Option<String> {
    let last = *chain.last()?;
    match adapter.fat_value(source, last) {
        Ok(FatValue::EndOfChain) => None,
        Ok(FatValue::Next(next)) if chain.contains(&next) => Some(format!(
            "cluster {last} points back to cluster {next}: the chain loops"
        )),
        Ok(FatValue::Next(_)) => None,
        Ok(FatValue::Free) => Some(format!("cluster {last} is followed by a free cluster")),
        Ok(FatValue::Bad) => Some(format!("cluster {last} is followed by a bad cluster")),
        Ok(FatValue::Reserved(value)) => Some(format!(
            "cluster {last} holds the reserved value 0x{value:X}"
        )),
        Ok(FatValue::OutOfRange(value)) => Some(format!(
            "cluster {last} points to {value}, outside the volume"
        )),
        Err(_) => Some(format!("the FAT entry of cluster {last} cannot be read")),
    }
}

/// The directories and files from the root.
fn namespace(adapter: &FatAdapter, source: &dyn BlockRead, allocated: &Bits, out: &mut Out<'_>) {
    let boot = adapter.boot();
    let cluster_size = adapter.cluster_size();
    let mut claimed = Bits::new(boot.cluster_count + 2);
    let mut visited = HashSet::new();
    let mut queue: VecDeque<u32> = VecDeque::from([adapter.root_first()]);
    let mut clean = true;
    while let Some(first) = queue.pop_front() {
        if !visited.insert(first) {
            continue;
        }
        let root = first == adapter.root_first();
        let name = if root { "root directory" } else { "directory" };
        // The directory's bytes, and where each cluster of them lies.
        let mut places: Vec<(u64, u64)> = Vec::new();
        let mut bytes = Vec::new();
        if first == 0 {
            let area = adapter.fixed_root();
            let (read, unreadable) = adapter.read_lenient(source, area);
            clean &= unreadable.is_empty();
            if !out.unreadable(unreadable, name, Severity::Error) {
                return;
            }
            places.push((0, area.offset));
            bytes = read;
        } else {
            let (chain, failure) = adapter.follow(source, first);
            if chain.is_empty() {
                clean = false;
                let finding = Damage {
                    kind: DamageKind::Malformed,
                    severity: Severity::Error,
                    location: adapter.root_extent(),
                    structure: name.into(),
                    detail: format!("the directory starts at cluster {first}, outside the volume"),
                };
                if !out.emit(finding) {
                    return;
                }
                continue;
            }
            if let Some(unreadable) = failure {
                clean = false;
                if !out.unreadable(vec![unreadable], "FAT", Severity::Error) {
                    return;
                }
            }
            if let Some(fault) = chain_fault(adapter, source, &chain) {
                clean = false;
                let finding = Damage {
                    kind: DamageKind::Broken,
                    severity: Severity::Error,
                    location: adapter.cluster_extent(*chain.last().unwrap()),
                    structure: name.into(),
                    detail: fault,
                };
                if !out.emit(finding) {
                    return;
                }
            }
            for cluster in &chain {
                if claimed.set(*cluster) {
                    clean = false;
                    let finding = crosslink(adapter, *cluster);
                    if !out.emit(finding) {
                        return;
                    }
                }
                let extent = adapter.cluster_extent(*cluster);
                let (read, unreadable) = adapter.read_lenient(source, extent);
                clean &= unreadable.is_empty();
                if !out.unreadable(unreadable, name, Severity::Error) {
                    return;
                }
                places.push((bytes.len() as u64, extent.offset));
                bytes.extend(read);
            }
        }
        let Ok(records) = scan_records(&bytes, boot.kind, false) else {
            continue;
        };
        let locate = |offset: usize| {
            let (start, at) = places
                .iter()
                .rev()
                .find(|(start, _)| *start <= offset as u64)
                .copied()
                .unwrap_or((0, 0));
            Extent::new(at + (offset as u64 - start), 32)
        };
        for scanned in records {
            let entry = &scanned.record.entry;
            let location = locate(scanned.record.offset);
            if scanned.long_name_ok == Some(false) && !entry.deleted {
                let finding = Damage {
                    kind: DamageKind::Checksum,
                    severity: Severity::Warning,
                    location,
                    structure: name.into(),
                    detail: format!(
                        "the long name entries before \"{}\" carry another short name's checksum",
                        entry.name
                    ),
                };
                if !out.emit(finding) {
                    return;
                }
            }
            if entry.deleted {
                continue;
            }
            if entry.name == "." {
                if first != 0 && entry.first_cluster != first {
                    clean = false;
                    let finding = Damage {
                        kind: DamageKind::Mismatch,
                        severity: Severity::Warning,
                        location,
                        structure: name.into(),
                        detail: format!(
                            "the \".\" entry names cluster {}, and the directory starts at {first}",
                            entry.first_cluster
                        ),
                    };
                    if !out.emit(finding) {
                        return;
                    }
                }
                continue;
            }
            if entry.name == ".." {
                continue;
            }
            let directory = entry.attributes & 0x10 != 0;
            if directory {
                if adapter.valid_cluster(entry.first_cluster) {
                    queue.push_back(entry.first_cluster);
                } else {
                    clean = false;
                    let finding = Damage {
                        kind: DamageKind::Malformed,
                        severity: Severity::Error,
                        location,
                        structure: name.into(),
                        detail: format!(
                            "directory \"{}\" starts at cluster {}, outside the volume",
                            entry.name, entry.first_cluster
                        ),
                    };
                    if !out.emit(finding) {
                        return;
                    }
                }
                continue;
            }
            if entry.size == 0 {
                continue;
            }
            let needed = u64::from(entry.size).div_ceil(cluster_size);
            let (chain, failure) = adapter.follow(source, entry.first_cluster);
            if let Some(unreadable) = failure {
                clean = false;
                if !out.unreadable(vec![unreadable], "FAT", Severity::Error) {
                    return;
                }
            }
            let problem = if chain.is_empty() {
                Some(format!(
                    "file \"{}\" starts at cluster {}, outside the volume",
                    entry.name, entry.first_cluster
                ))
            } else if (chain.len() as u64) < needed {
                let why = chain_fault(adapter, source, &chain).unwrap_or_default();
                Some(format!(
                    "file \"{}\" is {} bytes and needs {needed} clusters; its chain has {}: {why}",
                    entry.name,
                    entry.size,
                    chain.len()
                ))
            } else {
                None
            };
            if let Some(detail) = problem {
                clean = false;
                let finding = Damage {
                    kind: if chain.is_empty() {
                        DamageKind::Malformed
                    } else {
                        DamageKind::Broken
                    },
                    severity: Severity::Error,
                    location,
                    structure: name.into(),
                    detail,
                };
                if !out.emit(finding) {
                    return;
                }
            } else if chain.len() as u64 > needed {
                let finding = Damage {
                    kind: DamageKind::Mismatch,
                    severity: Severity::Warning,
                    location,
                    structure: name.into(),
                    detail: format!(
                        "file \"{}\" is {} bytes and needs {needed} clusters; its chain has {}",
                        entry.name,
                        entry.size,
                        chain.len()
                    ),
                };
                if !out.emit(finding) {
                    return;
                }
            }
            for cluster in chain {
                if claimed.set(cluster) {
                    clean = false;
                    let finding = crosslink(adapter, cluster);
                    if !out.emit(finding) {
                        return;
                    }
                }
            }
        }
    }
    if clean && !out.errors {
        lost_clusters(adapter, allocated, &claimed, out);
    }
}

fn crosslink(adapter: &FatAdapter, cluster: u32) -> Damage {
    Damage {
        kind: DamageKind::Mismatch,
        severity: Severity::Error,
        location: adapter.cluster_extent(cluster),
        structure: "FAT".into(),
        detail: format!("cluster {cluster} belongs to more than one chain"),
    }
}

/// Clusters the FAT calls allocated that no file or directory owns, in runs.
/// Said only of a namespace that walked without a fault, so a broken
/// directory does not turn its files into lost ones.
fn lost_clusters(adapter: &FatAdapter, allocated: &Bits, claimed: &Bits, out: &mut Out<'_>) {
    let boot = adapter.boot();
    let mut run: Option<(u32, u32)> = None;
    let flush = |run: Option<(u32, u32)>, out: &mut Out<'_>| -> bool {
        let Some((first, count)) = run else {
            return true;
        };
        let start = adapter.cluster_extent(first).offset;
        out.emit(Damage {
            kind: DamageKind::Mismatch,
            severity: Severity::Warning,
            location: Extent::new(start, u64::from(count) * adapter.cluster_size()),
            structure: "FAT".into(),
            detail: format!(
                "{count} allocated cluster(s) from {first} belong to no file or directory"
            ),
        })
    };
    for cluster in 2..boot.cluster_count + 2 {
        let lost = allocated.get(cluster) && !claimed.get(cluster);
        run = match (run, lost) {
            (Some((first, count)), true) => Some((first, count + 1)),
            (Some(_), false) => {
                if !flush(run, out) {
                    return;
                }
                None
            }
            (None, true) => Some((cluster, 1)),
            (None, false) => None,
        };
    }
    flush(run, out);
}
