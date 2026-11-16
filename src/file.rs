use crate::directory::{parse_directory, DirectoryEntry};
use crate::primitives::{FatValue, PrimitiveAPI};
use crate::volume::{Volume, VolumeAPI};
use crate::{Error, Result};
use fs_core::BlockRead;

pub trait FileAPI {
    fn root_directory(&self) -> Result<Vec<DirectoryEntry>>;
    fn open_file(&self, entry: &DirectoryEntry) -> Result<File<'_, Self::Device>>
    where
        Self: Sized;
    type Device: BlockRead;
}

impl<Device: BlockRead> FileAPI for Volume<Device> {
    type Device = Device;

    fn root_directory(&self) -> Result<Vec<DirectoryEntry>> {
        let boot = self.boot();
        if boot.kind == crate::FatKind::Fat32 {
            let cluster = self.root_cluster_bytes(boot.root_cluster)?;
            Ok(parse_directory(&cluster, boot.kind)?
                .into_iter()
                .filter(|entry| !entry.deleted)
                .collect())
        } else {
            let start = (u64::from(boot.reserved_sectors)
                + u64::from(boot.fat_count) * u64::from(boot.sectors_per_fat))
                * u64::from(boot.bytes_per_sector);
            let length = u64::from(boot.root_dir_sectors) * u64::from(boot.bytes_per_sector);
            let mut bytes = vec![0; usize::try_from(length).map_err(|_| Error::Bounds)?];
            self.read_volume_bytes(start, &mut bytes)?;
            Ok(parse_directory(&bytes, boot.kind)?
                .into_iter()
                .filter(|entry| !entry.deleted)
                .collect())
        }
    }

    fn open_file(&self, entry: &DirectoryEntry) -> Result<File<'_, Device>> {
        if entry.deleted {
            return Err(Error::Invalid("entry has been deleted"));
        }
        if entry.attributes & 0x10 != 0 {
            return Err(Error::Invalid("entry is a directory"));
        }
        let needed = (u64::from(entry.size).div_ceil(self.boot().cluster_size())) as usize;
        if needed == 0 {
            return Ok(File {
                volume: self,
                clusters: Vec::new(),
                length: 0,
            });
        }
        let candidates = self.chain_candidates(entry.first_cluster, needed)?;
        if candidates.len() < needed {
            return Err(Error::Invalid("file chain ends before file size"));
        }
        Ok(File {
            volume: self,
            clusters: candidates[..needed].to_vec(),
            length: u64::from(entry.size),
        })
    }
}

pub struct File<'volume, Device: BlockRead> {
    volume: &'volume Volume<Device>,
    clusters: Vec<u32>,
    length: u64,
}

impl<Device: BlockRead> File<'_, Device> {
    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn read_at(&self, offset: u64, output: &mut [u8]) -> Result<usize> {
        if offset >= self.length || output.is_empty() {
            return Ok(0);
        }
        let count = (self.length - offset).min(output.len() as u64) as usize;
        let cluster_size = self.volume.boot().cluster_size();
        let mut copied = 0;
        while copied < count {
            let position = offset + copied as u64;
            let index = (position / cluster_size) as usize;
            let within = position % cluster_size;
            let chunk = (cluster_size - within).min((count - copied) as u64) as usize;
            let cluster = *self
                .clusters
                .get(index)
                .ok_or(Error::Invalid("short file chain"))?;
            self.volume.read_volume_bytes(
                self.volume.cluster_offset(cluster)? + within,
                &mut output[copied..copied + chunk],
            )?;
            copied += chunk;
        }
        Ok(count)
    }
}

impl<Device: BlockRead> Volume<Device> {
    fn root_cluster_bytes(&self, first: u32) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut cluster = first;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..self.boot().cluster_count {
            if !seen.insert(cluster) {
                return Err(Error::Invalid("root directory chain loops"));
            }
            let mut data = vec![0; self.boot().cluster_size() as usize];
            self.read_cluster(cluster, &mut data)?;
            let ended = data.chunks_exact(32).any(|entry| entry[0] == 0);
            output.extend(data);
            if ended {
                break;
            }
            match self.fat_value(cluster)? {
                FatValue::Next(next) => cluster = next,
                FatValue::EndOfChain => break,
                _ => return Err(Error::Invalid("root directory chain is damaged")),
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDisk(Vec<u8>);

    impl BlockRead for TestDisk {
        fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core::Result<()> {
            let start = usize::try_from(offset).map_err(|_| fs_core::Error::OutOfBounds {
                offset,
                len: output.len() as u64,
                size: self.0.len() as u64,
            })?;
            let end = start
                .checked_add(output.len())
                .ok_or(fs_core::Error::OutOfBounds {
                    offset,
                    len: output.len() as u64,
                    size: self.0.len() as u64,
                })?;
            let data = self.0.get(start..end).ok_or(fs_core::Error::OutOfBounds {
                offset,
                len: output.len() as u64,
                size: self.0.len() as u64,
            })?;
            output.copy_from_slice(data);
            Ok(())
        }

        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
    }

    #[test]
    fn file_api_lists_and_reads_a_fat12_root_file() {
        let mut bytes = vec![0; 4000 * 512];
        let boot = &mut bytes[..512];
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = 1;
        boot[14..16].copy_from_slice(&1u16.to_le_bytes());
        boot[16] = 2;
        boot[17..19].copy_from_slice(&224u16.to_le_bytes());
        boot[19..21].copy_from_slice(&4000u16.to_le_bytes());
        boot[22..24].copy_from_slice(&12u16.to_le_bytes());
        boot[510..512].copy_from_slice(&[0x55, 0xAA]);
        let fat = &mut bytes[512..512 + 12 * 512];
        fat[0..5].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0xFF, 0x0F]);
        let root = &mut bytes[25 * 512..25 * 512 + 512];
        root[..11].copy_from_slice(b"HELLO   TXT");
        root[11] = 0x20;
        root[26..28].copy_from_slice(&2u16.to_le_bytes());
        root[28..32].copy_from_slice(&5u32.to_le_bytes());
        bytes[39 * 512..39 * 512 + 5].copy_from_slice(b"hello");

        let volume = Volume::open(TestDisk(bytes)).unwrap();
        let root = volume.root_directory().unwrap();
        assert_eq!(root[0].name, "HELLO.TXT");
        let file = volume.open_file(&root[0]).unwrap();
        let mut content = [0; 5];
        assert_eq!(file.read_at(0, &mut content).unwrap(), 5);
        assert_eq!(&content, b"hello");
    }

    #[test]
    fn file_api_reads_fat16_fixed_root_and_fat32_cluster_root() {
        let mut fat16 = vec![0; 5000 * 512];
        let boot = &mut fat16[..512];
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = 1;
        boot[14..16].copy_from_slice(&1u16.to_le_bytes());
        boot[16] = 2;
        boot[17..19].copy_from_slice(&512u16.to_le_bytes());
        boot[19..21].copy_from_slice(&5000u16.to_le_bytes());
        boot[22..24].copy_from_slice(&20u16.to_le_bytes());
        boot[510..512].copy_from_slice(&[0x55, 0xAA]);
        fat16[516..518].copy_from_slice(&0xFFFFu16.to_le_bytes());
        let root = &mut fat16[41 * 512..41 * 512 + 512];
        root[..11].copy_from_slice(b"FAT16   TXT");
        root[11] = 0x20;
        root[26..28].copy_from_slice(&2u16.to_le_bytes());
        root[28..32].copy_from_slice(&5u32.to_le_bytes());
        fat16[73 * 512..73 * 512 + 7].copy_from_slice(b"sixteen");

        let volume = Volume::open(TestDisk(fat16)).unwrap();
        assert_eq!(volume.boot().kind, crate::FatKind::Fat16);
        let entry = volume.root_directory().unwrap().remove(0);
        let file = volume.open_file(&entry).unwrap();
        let mut content = [0; 5];
        file.read_at(0, &mut content).unwrap();
        assert_eq!(&content, b"sixte");

        let mut fat32 = vec![0; 67_000 * 512];
        let boot = &mut fat32[..512];
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = 1;
        boot[14..16].copy_from_slice(&32u16.to_le_bytes());
        boot[16] = 2;
        boot[32..36].copy_from_slice(&67_000u32.to_le_bytes());
        boot[36..40].copy_from_slice(&520u32.to_le_bytes());
        boot[44..48].copy_from_slice(&2u32.to_le_bytes());
        boot[510..512].copy_from_slice(&[0x55, 0xAA]);
        let fat_start = 32 * 512;
        fat32[fat_start + 8..fat_start + 12].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
        fat32[fat_start + 12..fat_start + 16].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
        let root_sector = 32 + 2 * 520;
        let root = &mut fat32[root_sector * 512..root_sector * 512 + 512];
        root[..11].copy_from_slice(b"FAT32   TXT");
        root[11] = 0x20;
        root[26..28].copy_from_slice(&3u16.to_le_bytes());
        root[28..32].copy_from_slice(&5u32.to_le_bytes());
        fat32[(root_sector + 1) * 512..(root_sector + 1) * 512 + 6].copy_from_slice(b"thirty");

        let volume = Volume::open(TestDisk(fat32)).unwrap();
        assert_eq!(volume.boot().kind, crate::FatKind::Fat32);
        let entry = volume.root_directory().unwrap().remove(0);
        let file = volume.open_file(&entry).unwrap();
        let mut content = [0; 5];
        file.read_at(0, &mut content).unwrap();
        assert_eq!(&content, b"thirt");
    }

    #[test]
    fn raw_volume_api_reads_bytes_without_a_valid_boot_sector() {
        let raw = crate::RawVolume::open(TestDisk(vec![0xA5; 512]));
        let mut bytes = [0; 4];
        raw.read_volume_bytes(10, &mut bytes).unwrap();
        assert_eq!(bytes, [0xA5; 4]);
        assert!(raw.read_volume_bytes(510, &mut bytes).is_err());
    }
}
