//! The FAT as the writer uses it: reading entries, following chains,
//! counting and allocating free clusters, and keeping the FAT32 FSInfo hints.

use super::WriteVolume;
use crate::boot::FatKind;
use crate::primitives::{classify_fat_value, FatValue};
use crate::volume::VolumeAPI;
use crate::{Error, Result};
use fs_core::BlockDevice;

/// Entries read from the FAT in one go while scanning.
const SCAN_ENTRIES: u32 = 16_384;
/// Bytes of the FAT held while following a chain.
const WINDOW: u64 = 8192;

/// The FSInfo sector of a FAT32 volume and what was last written to it.
pub(crate) struct FsInfo {
    /// Byte offset of the sector in the volume.
    pub offset: u64,
    pub written_free: u32,
    pub written_next: u32,
}

/// A piece of the active FAT kept while a chain is followed.
struct Window {
    start: u64,
    bytes: Vec<u8>,
}

pub(crate) fn entry_span(kind: FatKind, cluster: u32) -> (u64, u64) {
    match kind {
        FatKind::Fat12 => (u64::from(cluster) * 3 / 2, 2),
        FatKind::Fat16 => (u64::from(cluster) * 2, 2),
        FatKind::Fat32 => (u64::from(cluster) * 4, 4),
    }
}

fn decode(kind: FatKind, bytes: &[u8], at: usize, cluster: u32) -> u32 {
    match kind {
        FatKind::Fat12 => {
            let word = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
            if cluster & 1 == 0 {
                u32::from(word & 0x0FFF)
            } else {
                u32::from(word >> 4)
            }
        }
        FatKind::Fat16 => u32::from(u16::from_le_bytes([bytes[at], bytes[at + 1]])),
        FatKind::Fat32 => {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
                & 0x0FFF_FFFF
        }
    }
}

impl<Device: BlockDevice> WriteVolume<Device> {
    /// The end-of-chain mark for the variant.
    pub(crate) fn end_mark(&self) -> u32 {
        match self.volume.boot().kind {
            FatKind::Fat12 => 0xFFF,
            FatKind::Fat16 => 0xFFFF,
            FatKind::Fat32 => 0x0FFF_FFFF,
        }
    }

    fn active_fat_offset(&self) -> u64 {
        let boot = self.volume.boot();
        (u64::from(boot.reserved_sectors)
            + u64::from(boot.active_fat) * u64::from(boot.sectors_per_fat))
            * u64::from(boot.bytes_per_sector)
    }

    /// `count` consecutive FAT entries from `first`, from the active FAT.
    pub(crate) fn fat_entries(&self, first: u32, count: u32) -> Result<Vec<u32>> {
        let boot = self.volume.boot();
        let kind = boot.kind;
        if count == 0 {
            return Ok(Vec::new());
        }
        let last = first + count - 1;
        if last >= boot.cluster_count + 2 {
            return Err(Error::Bounds);
        }
        let (start, _) = entry_span(kind, first);
        let (end, length) = entry_span(kind, last);
        let mut bytes = vec![0u8; (end + length - start) as usize];
        self.volume
            .read_volume_bytes(self.active_fat_offset() + start, &mut bytes)?;
        Ok((first..=last)
            .map(|cluster| {
                let (at, _) = entry_span(kind, cluster);
                decode(kind, &bytes, (at - start) as usize, cluster)
            })
            .collect())
    }

    fn fat_value_in(&self, window: &mut Window, cluster: u32) -> Result<u32> {
        let boot = self.volume.boot();
        let kind = boot.kind;
        let (at, length) = entry_span(kind, cluster);
        let fat_bytes = u64::from(boot.sectors_per_fat) * u64::from(boot.bytes_per_sector);
        if at + length > fat_bytes {
            return Err(Error::Bounds);
        }
        if at < window.start || at + length > window.start + window.bytes.len() as u64 {
            let start = at / 4096 * 4096;
            let size = WINDOW.min(fat_bytes - start);
            window.bytes = vec![0u8; size as usize];
            window.start = start;
            self.volume
                .read_volume_bytes(self.active_fat_offset() + start, &mut window.bytes)?;
        }
        Ok(decode(
            kind,
            &window.bytes,
            (at - window.start) as usize,
            cluster,
        ))
    }

    /// The chain starting at `first` as far as it can be followed, and what
    /// stopped it if that was not a proper end: a chain that leaves the data
    /// area, runs into a free, reserved or bad entry, or loops. Only a device
    /// error is an `Err`.
    fn follow_chain(&self, first: u32) -> Result<(Vec<u32>, Option<Error>)> {
        let boot = self.volume.boot();
        let mut chain = Vec::new();
        if first == 0 {
            return Ok((chain, None));
        }
        let mut window = Window {
            start: 0,
            bytes: Vec::new(),
        };
        let mut current = first;
        loop {
            if current < 2 || current >= boot.cluster_count + 2 {
                return Ok((
                    chain,
                    Some(Error::Invalid("cluster chain leaves the data area")),
                ));
            }
            if chain.len() as u64 >= u64::from(boot.cluster_count) {
                return Ok((chain, Some(Error::Invalid("cluster chain loops"))));
            }
            chain.push(current);
            let raw = self.fat_value_in(&mut window, current)?;
            match classify_fat_value(boot.kind, raw, boot.cluster_count + 1) {
                FatValue::Next(next) => current = next,
                FatValue::EndOfChain => return Ok((chain, None)),
                _ => return Ok((chain, Some(Error::Invalid("cluster chain is broken")))),
            }
        }
    }

    /// The whole chain starting at `first`, in order; empty for cluster 0. A
    /// damaged chain is an error.
    pub(crate) fn chain(&self, first: u32) -> Result<Vec<u32>> {
        match self.follow_chain(first)? {
            (chain, None) => Ok(chain),
            (_, Some(error)) => Err(error),
        }
    }

    /// The chain as far as it can be followed, for removing a file whose
    /// chain is damaged: what is sound is still freed.
    pub(crate) fn chain_prefix(&self, first: u32) -> Result<Vec<u32>> {
        Ok(self.follow_chain(first)?.0)
    }

    /// Counts the free clusters of the active FAT.
    pub(crate) fn scan_free(&self) -> Result<u32> {
        let boot = self.volume.boot();
        let mut free = 0u32;
        let mut cluster = 2u32;
        let end = boot.cluster_count + 2;
        while cluster < end {
            let count = SCAN_ENTRIES.min(end - cluster);
            free += self
                .fat_entries(cluster, count)?
                .iter()
                .filter(|entry| **entry == 0)
                .count() as u32;
            cluster += count;
        }
        Ok(free)
    }

    /// Finds `count` free clusters, searching on from the last allocation and
    /// wrapping round. The FAT is not changed: link them with
    /// [`link_chain`](Self::link_chain).
    pub(crate) fn allocate(&mut self, count: u32) -> Result<Vec<u32>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        if count > self.free {
            return Err(Error::VolumeFull);
        }
        let end = self.volume.boot().cluster_count + 2;
        let start = if (2..end).contains(&self.next_free) {
            self.next_free
        } else {
            2
        };
        let mut found = Vec::with_capacity(count as usize);
        for (from, to) in [(start, end), (2, start)] {
            let mut cluster = from;
            while cluster < to && (found.len() as u32) < count {
                let span = SCAN_ENTRIES.min(to - cluster);
                for (index, entry) in self.fat_entries(cluster, span)?.into_iter().enumerate() {
                    if entry == 0 {
                        found.push(cluster + index as u32);
                        if found.len() as u32 == count {
                            break;
                        }
                    }
                }
                cluster += span;
            }
        }
        if (found.len() as u32) < count {
            return Err(Error::Invalid(
                "the free cluster count disagrees with the FAT",
            ));
        }
        self.free -= count;
        let last = *found.last().expect("at least one cluster");
        self.next_free = if last + 1 >= end { 2 } else { last + 1 };
        Ok(found)
    }

    /// Links `new` clusters into one chain ending the file, after `previous`
    /// when the file already has clusters.
    pub(crate) fn link_chain(&mut self, previous: Option<u32>, new: &[u32]) -> Result<()> {
        if new.is_empty() {
            return Ok(());
        }
        let mut updates = Vec::with_capacity(new.len() + 1);
        if let Some(previous) = previous {
            updates.push((previous, new[0]));
        }
        for pair in new.windows(2) {
            updates.push((pair[0], pair[1]));
        }
        updates.push((*new.last().expect("not empty"), self.end_mark()));
        self.write_fat_entries(&updates)
    }

    /// Frees `clusters`, which must be the tail (or all) of a chain.
    pub(crate) fn free_clusters_of(&mut self, clusters: &[u32]) -> Result<()> {
        let updates: Vec<(u32, u32)> = clusters.iter().map(|cluster| (*cluster, 0)).collect();
        self.write_fat_entries(&updates)?;
        self.free += clusters.len() as u32;
        Ok(())
    }

    /// Cuts a chain to its first `keep` clusters (at least one) and frees the
    /// rest, ending the kept part first in the same write.
    pub(crate) fn shrink_chain(&mut self, chain: &[u32], keep: usize) -> Result<()> {
        if keep >= chain.len() {
            return Ok(());
        }
        let mut updates = Vec::with_capacity(chain.len() - keep + 1);
        updates.push((chain[keep - 1], self.end_mark()));
        updates.extend(chain[keep..].iter().map(|cluster| (*cluster, 0)));
        self.write_fat_entries(&updates)?;
        self.free += (chain.len() - keep) as u32;
        Ok(())
    }

    /// Writes the FAT32 FSInfo free count and next-free hint when they changed.
    pub(crate) fn write_fsinfo(&mut self) -> Result<()> {
        let end = self.volume.boot().cluster_count + 2;
        let next = if (2..end).contains(&self.next_free) {
            self.next_free
        } else {
            0xFFFF_FFFF
        };
        let free = self.free;
        let Some(info) = self.fsinfo.as_ref() else {
            return Ok(());
        };
        if info.written_free == free && info.written_next == next {
            return Ok(());
        }
        let offset = info.offset + 488;
        let mut bytes = [0u8; 8];
        bytes[..4].copy_from_slice(&free.to_le_bytes());
        bytes[4..].copy_from_slice(&next.to_le_bytes());
        self.write_volume(offset, &bytes)?;
        if let Some(info) = self.fsinfo.as_mut() {
            info.written_free = free;
            info.written_next = next;
        }
        Ok(())
    }
}
