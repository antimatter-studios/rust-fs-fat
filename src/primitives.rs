use crate::boot::{BootSector, FatKind};
#[allow(deprecated)]
use crate::volume::{Volume, VolumeAPI};
use crate::{Error, Result};
use fs_core::BlockRead;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FatValue {
    Next(u32),
    EndOfChain,
    Free,
    Reserved(u32),
    Bad,
    OutOfRange(u32),
}

/// The primitive layer this crate had before `fs_core::layers`.
///
/// Superseded by `fs_core::layers::PrimitiveAPI`, which [`Volume`] now also
/// implements, with the FAT-specific reads below as inherent methods of
/// [`Volume`]. This trait stays for one release as a thin wrapper over them.
#[deprecated(
    since = "0.2.0",
    note = "use the same methods on fs_fat32::Volume and fs_core::layers::PrimitiveAPI; removed in the next release"
)]
#[allow(deprecated)]
pub trait PrimitiveAPI: VolumeAPI {
    fn fat_entry_raw(&self, cluster: u32) -> Result<u32>;
    fn fat_value(&self, cluster: u32) -> Result<FatValue>;
    fn chain_candidates(&self, first: u32, limit: usize) -> Result<Vec<u32>>;
}

pub fn classify_fat_value(kind: FatKind, value: u32, max_cluster: u32) -> FatValue {
    let (reserved, bad, end) = match kind {
        FatKind::Fat12 => (0x0FF0, 0x0FF7, 0x0FF8),
        FatKind::Fat16 => (0xFFF0, 0xFFF7, 0xFFF8),
        FatKind::Fat32 => (0x0FFF_FFF0, 0x0FFF_FFF7, 0x0FFF_FFF8),
    };
    if value == 0 {
        FatValue::Free
    } else if value == bad {
        FatValue::Bad
    } else if value >= end {
        FatValue::EndOfChain
    } else if value >= reserved {
        FatValue::Reserved(value)
    } else if value < 2 || value > max_cluster {
        FatValue::OutOfRange(value)
    } else {
        FatValue::Next(value)
    }
}

pub fn decode_fat_entry(kind: FatKind, bytes: &[u8], cluster: u32) -> Result<u32> {
    let index = match kind {
        FatKind::Fat12 => usize::try_from(u64::from(cluster) + u64::from(cluster) / 2)
            .map_err(|_| Error::Bounds)?,
        FatKind::Fat16 => usize::try_from(u64::from(cluster) * 2).map_err(|_| Error::Bounds)?,
        FatKind::Fat32 => usize::try_from(u64::from(cluster) * 4).map_err(|_| Error::Bounds)?,
    };
    let need = if kind == FatKind::Fat32 { 4 } else { 2 };
    let pair = bytes.get(index..index + need).ok_or(Error::Bounds)?;
    let value = match kind {
        FatKind::Fat12 => {
            let word = u16::from_le_bytes([pair[0], pair[1]]);
            if cluster & 1 == 0 {
                u32::from(word & 0x0FFF)
            } else {
                u32::from(word >> 4)
            }
        }
        FatKind::Fat16 => u32::from(u16::from_le_bytes([pair[0], pair[1]])),
        FatKind::Fat32 => u32::from_le_bytes(pair[..4].try_into().unwrap()) & 0x0FFF_FFFF,
    };
    Ok(value)
}

impl<Device: BlockRead> Volume<Device> {
    /// The raw value of a cluster's entry in the active FAT.
    pub fn fat_entry_raw(&self, cluster: u32) -> Result<u32> {
        self.fat_entry_raw_in(self.boot().active_fat, cluster)
    }

    /// The raw value of a cluster's entry in FAT copy `copy`.
    pub fn fat_entry_raw_in(&self, copy: u8, cluster: u32) -> Result<u32> {
        let boot = self.boot();
        let fat_offset = (u64::from(boot.reserved_sectors)
            + u64::from(copy) * u64::from(boot.sectors_per_fat))
            * u64::from(boot.bytes_per_sector);
        let fat_size = u64::from(boot.sectors_per_fat) * u64::from(boot.bytes_per_sector);
        let entry_offset = match boot.kind {
            FatKind::Fat12 => (u64::from(cluster) * 3) / 2,
            FatKind::Fat16 => u64::from(cluster) * 2,
            FatKind::Fat32 => u64::from(cluster) * 4,
        };
        let size = if boot.kind == FatKind::Fat32 { 4 } else { 2 };
        if entry_offset + size > fat_size {
            return Err(Error::Bounds);
        }
        let mut entry = [0; 4];
        self.read_bytes(fat_offset + entry_offset, &mut entry[..size as usize])?;
        let value = match boot.kind {
            FatKind::Fat12 => {
                let word = u16::from_le_bytes([entry[0], entry[1]]);
                if cluster & 1 == 0 {
                    u32::from(word & 0x0FFF)
                } else {
                    u32::from(word >> 4)
                }
            }
            FatKind::Fat16 => u32::from(u16::from_le_bytes([entry[0], entry[1]])),
            FatKind::Fat32 => u32::from_le_bytes(entry) & 0x0FFF_FFFF,
        };
        Ok(value)
    }

    /// A cluster's entry in the active FAT, classified.
    pub fn fat_value(&self, cluster: u32) -> Result<FatValue> {
        let boot: &BootSector = self.boot();
        Ok(classify_fat_value(
            boot.kind,
            self.fat_entry_raw(cluster)?,
            boot.cluster_count + 1,
        ))
    }

    /// The clusters of a chain from `first`, at most `limit` of them, ending
    /// where the chain ends, leaves the volume or loops.
    pub fn chain_candidates(&self, first: u32, limit: usize) -> Result<Vec<u32>> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut current = first;
        while chain.len() < limit {
            if current < 2 || current >= self.boot().cluster_count + 2 || !seen.insert(current) {
                break;
            }
            chain.push(current);
            match self.fat_value(current)? {
                FatValue::Next(next) => current = next,
                _ => break,
            }
        }
        Ok(chain)
    }
}

#[allow(deprecated)]
impl<Device: BlockRead> PrimitiveAPI for Volume<Device> {
    fn fat_entry_raw(&self, cluster: u32) -> Result<u32> {
        Volume::fat_entry_raw(self, cluster)
    }

    fn fat_value(&self, cluster: u32) -> Result<FatValue> {
        Volume::fat_value(self, cluster)
    }

    fn chain_candidates(&self, first: u32, limit: usize) -> Result<Vec<u32>> {
        Volume::chain_candidates(self, first, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_packed_fat12_entries() {
        let bytes = [0x23, 0x61, 0x45];
        assert_eq!(decode_fat_entry(FatKind::Fat12, &bytes, 0).unwrap(), 0x123);
        assert_eq!(decode_fat_entry(FatKind::Fat12, &bytes, 1).unwrap(), 0x456);
    }

    #[test]
    fn masks_fat32_reserved_high_bits() {
        assert_eq!(
            decode_fat_entry(FatKind::Fat32, &[0x34, 0x12, 0, 0xF8], 0).unwrap(),
            0x0800_1234
        );
    }
}
