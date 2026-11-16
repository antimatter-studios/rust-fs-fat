use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FatKind {
    Fat12,
    Fat16,
    Fat32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootSector {
    pub volume_serial: u32,
    pub hidden_sectors: u32,
    pub active_fat: u8,
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub fat_count: u8,
    pub root_entry_count: u16,
    pub total_sectors: u32,
    pub sectors_per_fat: u32,
    pub root_cluster: u32,
    pub kind: FatKind,
    pub cluster_count: u32,
    pub root_dir_sectors: u32,
    pub first_data_sector: u32,
}

impl BootSector {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 512 {
            return Err(Error::Invalid("boot sector is shorter than 512 bytes"));
        }
        if bytes[510..512] != [0x55, 0xAA] {
            return Err(Error::Invalid("missing boot-sector signature"));
        }
        let bytes_per_sector = u16::from_le_bytes([bytes[11], bytes[12]]);
        let sectors_per_cluster = bytes[13];
        let reserved_sectors = u16::from_le_bytes([bytes[14], bytes[15]]);
        let fat_count = bytes[16];
        let root_entry_count = u16::from_le_bytes([bytes[17], bytes[18]]);
        let total16 = u16::from_le_bytes([bytes[19], bytes[20]]) as u32;
        let sectors_per_fat16 = u16::from_le_bytes([bytes[22], bytes[23]]) as u32;
        let total32 = u32::from_le_bytes(bytes[32..36].try_into().unwrap());
        let sectors_per_fat32 = u32::from_le_bytes(bytes[36..40].try_into().unwrap());
        let total_sectors = if total16 == 0 { total32 } else { total16 };
        let root_dir_sectors =
            (u32::from(root_entry_count) * 32).div_ceil(u32::from(bytes_per_sector.max(1)));
        let sectors_per_fat = if sectors_per_fat16 == 0 {
            sectors_per_fat32
        } else {
            sectors_per_fat16
        };

        if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
            return Err(Error::Invalid("unsupported bytes-per-sector value"));
        }
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
            return Err(Error::Invalid("sectors-per-cluster is not a power of two"));
        }
        if reserved_sectors == 0 || fat_count == 0 || sectors_per_fat == 0 || total_sectors == 0 {
            return Err(Error::Invalid("required BPB geometry is zero"));
        }
        let fat_area = u32::from(fat_count)
            .checked_mul(sectors_per_fat)
            .ok_or(Error::Bounds)?;
        let first_data_sector = u32::from(reserved_sectors)
            .checked_add(fat_area)
            .and_then(|n| n.checked_add(root_dir_sectors))
            .ok_or(Error::Bounds)?;
        if first_data_sector >= total_sectors {
            return Err(Error::Invalid("data region starts beyond volume"));
        }
        let cluster_count = (total_sectors - first_data_sector) / u32::from(sectors_per_cluster);
        let kind = match cluster_count {
            0..=4084 => FatKind::Fat12,
            4085..=65524 => FatKind::Fat16,
            _ => FatKind::Fat32,
        };
        let root_cluster = if kind == FatKind::Fat32 {
            u32::from_le_bytes(bytes[44..48].try_into().unwrap()) & 0x0FFF_FFFF
        } else {
            0
        };
        if kind == FatKind::Fat32 && sectors_per_fat16 != 0 {
            return Err(Error::Invalid("FAT32 BPB has legacy FAT size set"));
        }
        if kind != FatKind::Fat32 && root_entry_count == 0 {
            return Err(Error::Invalid("FAT12/16 BPB has no fixed root directory"));
        }
        if kind == FatKind::Fat32 && root_entry_count != 0 {
            return Err(Error::Invalid("FAT32 BPB has fixed root entries"));
        }
        let fat_bytes = u64::from(sectors_per_fat) * u64::from(bytes_per_sector);
        let fat_entries = u64::from(cluster_count) + 2;
        let required_fat_bytes = match kind {
            FatKind::Fat12 => (fat_entries * 3).div_ceil(2),
            FatKind::Fat16 => fat_entries * 2,
            FatKind::Fat32 => fat_entries * 4,
        };
        if required_fat_bytes > fat_bytes {
            return Err(Error::Invalid("FAT is too small for the data region"));
        }
        if kind == FatKind::Fat32 && (root_cluster < 2 || root_cluster >= cluster_count + 2) {
            return Err(Error::Invalid("FAT32 root cluster is outside data region"));
        }
        let active_fat = if kind == FatKind::Fat32 && bytes[40] & 0x80 != 0 {
            bytes[40] & 0x0F
        } else {
            0
        };
        if active_fat >= fat_count {
            return Err(Error::Invalid("active FAT index is outside FAT copies"));
        }
        let serial_offset = if kind == FatKind::Fat32 { 67 } else { 39 };
        Ok(Self {
            volume_serial: u32::from_le_bytes(
                bytes[serial_offset..serial_offset + 4].try_into().unwrap(),
            ),
            hidden_sectors: u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            active_fat,
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            fat_count,
            root_entry_count,
            total_sectors,
            sectors_per_fat,
            root_cluster,
            kind,
            cluster_count,
            root_dir_sectors,
            first_data_sector,
        })
    }

    pub fn cluster_size(&self) -> u64 {
        u64::from(self.bytes_per_sector) * u64::from(self.sectors_per_cluster)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot(total_sectors: u32, fat_size: u32, root_entries: u16, fat32: bool) -> Vec<u8> {
        let mut bytes = vec![0; 512];
        bytes[11..13].copy_from_slice(&512u16.to_le_bytes());
        bytes[13] = 1;
        bytes[14..16].copy_from_slice(&1u16.to_le_bytes());
        bytes[16] = 2;
        bytes[17..19].copy_from_slice(&root_entries.to_le_bytes());
        if fat32 {
            bytes[32..36].copy_from_slice(&total_sectors.to_le_bytes());
            bytes[36..40].copy_from_slice(&fat_size.to_le_bytes());
            bytes[44..48].copy_from_slice(&2u32.to_le_bytes());
        } else {
            bytes[19..21].copy_from_slice(&(total_sectors as u16).to_le_bytes());
            bytes[22..24].copy_from_slice(&(fat_size as u16).to_le_bytes());
        }
        bytes[510..512].copy_from_slice(&[0x55, 0xAA]);
        bytes
    }

    #[test]
    fn identifies_all_three_fat_variants_by_cluster_count() {
        assert_eq!(
            BootSector::parse(&boot(4000, 12, 224, false)).unwrap().kind,
            FatKind::Fat12
        );
        assert_eq!(
            BootSector::parse(&boot(10000, 40, 512, false))
                .unwrap()
                .kind,
            FatKind::Fat16
        );
        assert_eq!(
            BootSector::parse(&boot(100000, 800, 0, true)).unwrap().kind,
            FatKind::Fat32
        );
    }
}
