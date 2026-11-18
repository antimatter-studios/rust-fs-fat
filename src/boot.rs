use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FatKind {
    Fat12,
    Fat16,
    Fat32,
}

/// A defect in a boot sector that the reader tolerates and reports.
///
/// The readers accept these and carry on; a writer refuses them, because a
/// volume that is to be changed is first made right.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BootWarning {
    /// Bytes 510 and 511 are not `0x55 0xAA`. They are `found`.
    MissingSignature { found: [u8; 2] },
}

impl std::fmt::Display for BootWarning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSignature { found } => write!(
                formatter,
                "the boot sector's 0x55 0xAA signature is missing (bytes 510 and 511 are \
                 0x{:02X} 0x{:02X}); the rest of the BPB is consistent, so the volume is read as is",
                found[0], found[1]
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootSector {
    /// The two bytes at offset 510.
    pub signature: [u8; 2],
    /// Whether [`signature`](Self::signature) is `0x55 0xAA`.
    pub signature_ok: bool,
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
    /// Reads the BPB of a boot sector and checks that its geometry holds.
    ///
    /// The `0x55 0xAA` signature is not required. `fsck.fat` and the Linux
    /// kernel's vfat driver accept a volume whose BPB is intact and whose
    /// signature is zero, and a recovery tool has to read such a volume, so
    /// this does too and reports it: [`BootSector::signature_ok`] is false and
    /// [`BootSector::warnings`] says so. Without a signature the BPB is the
    /// only evidence that the sector is a boot sector, so it must also carry a
    /// media descriptor the kernel and mtools accept (0xF0, or 0xF8 to 0xFF);
    /// every geometry check applies as it does to a signed sector. A sector
    /// that fails any of them is refused as "missing boot-sector signature",
    /// the reason an all-zero sector has always been refused for. A writer
    /// does not take the lenient reading: it refuses an unsigned volume.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 512 {
            return Err(Error::Invalid("boot sector is shorter than 512 bytes"));
        }
        let signature = [bytes[510], bytes[511]];
        let signature_ok = signature == [0x55, 0xAA];
        match Self::parse_bpb(bytes, signature, signature_ok) {
            Err(_) if !signature_ok => Err(Error::Invalid("missing boot-sector signature")),
            parsed => parsed,
        }
    }

    fn parse_bpb(bytes: &[u8], signature: [u8; 2], signature_ok: bool) -> Result<Self> {
        if !(signature_ok || bytes[21] == 0xF0 || bytes[21] >= 0xF8) {
            return Err(Error::Invalid(
                "unsigned boot sector has no valid media byte",
            ));
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
            signature,
            signature_ok,
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

    /// What is wrong with this boot sector that the reader tolerates. Empty
    /// for a boot sector with nothing to report.
    pub fn warnings(&self) -> Vec<BootWarning> {
        let mut warnings = Vec::new();
        if !self.signature_ok {
            warnings.push(BootWarning::MissingSignature {
                found: self.signature,
            });
        }
        warnings
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
        bytes[21] = 0xF8;
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

    fn unsigned(mut bytes: Vec<u8>) -> Vec<u8> {
        bytes[510..512].fill(0);
        bytes
    }

    #[test]
    fn a_missing_signature_with_a_consistent_bpb_parses_and_is_reported() {
        for (signed, kind) in [
            (boot(4000, 12, 224, false), FatKind::Fat12),
            (boot(10000, 40, 512, false), FatKind::Fat16),
            (boot(100000, 800, 0, true), FatKind::Fat32),
        ] {
            let with = BootSector::parse(&signed).unwrap();
            assert!(with.signature_ok && with.warnings().is_empty());
            let without = BootSector::parse(&unsigned(signed.clone()))
                .unwrap_or_else(|error| panic!("{kind:?}: {error}"));
            assert_eq!(without.kind, kind);
            assert!(!without.signature_ok);
            assert_eq!(without.signature, [0, 0]);
            assert_eq!(
                without.warnings(),
                vec![BootWarning::MissingSignature { found: [0, 0] }]
            );
            // Nothing but the signature differs.
            let same = BootSector {
                signature: with.signature,
                signature_ok: true,
                ..without
            };
            assert_eq!(same, with);
        }
    }

    #[test]
    fn any_other_two_bytes_are_a_missing_signature_too() {
        for found in [[0x55, 0x00], [0x00, 0xAA], [0xAA, 0x55], [0xFF, 0xFF]] {
            let mut bytes = boot(10000, 40, 512, false);
            bytes[510..512].copy_from_slice(&found);
            let parsed = BootSector::parse(&bytes).unwrap();
            assert_eq!(
                parsed.warnings(),
                vec![BootWarning::MissingSignature { found }]
            );
        }
    }

    #[test]
    fn a_missing_signature_never_excuses_a_boot_sector_that_is_not_one() {
        // Not a boot sector at all.
        for bytes in [vec![0u8; 512], vec![0xFF; 512], vec![0xA5; 512]] {
            assert!(BootSector::parse(&bytes).is_err());
        }
        // A BPB whose geometry does not hold, signature or not.
        type Damage = fn(&mut Vec<u8>);
        let damages: [(&str, Damage); 9] = [
            ("bytes per sector", |b| {
                b[11..13].copy_from_slice(&0u16.to_le_bytes())
            }),
            ("odd bytes per sector", |b| {
                b[11..13].copy_from_slice(&300u16.to_le_bytes())
            }),
            ("sectors per cluster", |b| b[13] = 3),
            ("zero sectors per cluster", |b| b[13] = 0),
            ("reserved sectors", |b| b[14..16].fill(0)),
            ("FAT count", |b| b[16] = 0),
            ("FAT size", |b| {
                b[22..24].copy_from_slice(&1u16.to_le_bytes())
            }),
            ("total sectors", |b| b[19..21].fill(0)),
            ("media byte", |b| b[21] = 0x00),
        ];
        for (what, damage) in damages {
            let mut bytes = boot(10000, 40, 512, false);
            damage(&mut bytes);
            let signed = BootSector::parse(&bytes);
            bytes[510..512].fill(0);
            let unsigned = BootSector::parse(&bytes);
            if what != "media byte" {
                assert!(signed.is_err(), "{what}: refused with its signature");
            }
            assert!(unsigned.is_err(), "{what}: refused without its signature");
        }
        // FAT32 geometry: a root cluster outside the data region.
        let mut bytes = boot(100000, 800, 0, true);
        bytes[44..48].copy_from_slice(&0u32.to_le_bytes());
        bytes[510..512].fill(0);
        assert!(BootSector::parse(&bytes).is_err());
    }

    #[test]
    fn a_missing_signature_keeps_the_reason_it_was_always_refused_for() {
        let error = BootSector::parse(&[0u8; 512]).unwrap_err();
        assert!(error.to_string().contains("missing boot-sector signature"));
    }
}
