//! Directories as the writer sees them: the whole directory read into memory
//! as one stream of 32-byte entries, parsed into entries with their long
//! names, searched for names and for room, and written back in pieces.

use super::names;
use super::WriteVolume;
use crate::boot::FatKind;
use crate::directory::{short_checksum, short_name};
use crate::{Error, Result};
use fs_core::BlockDevice;

/// A directory's entries are limited to 65536 (2 MiB).
const MAX_ENTRIES: usize = 65_536;

/// Which directory: 0 is the fixed root of FAT12 and FAT16, any other number
/// is the first cluster of the directory (for FAT32, the root's too).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DirId(pub u32);

pub(crate) struct DirStream {
    pub id: DirId,
    pub bytes: Vec<u8>,
    /// The clusters of the directory, empty for a fixed root.
    pub clusters: Vec<u32>,
}

/// A directory entry with the long-name entries before it.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    /// Offset in the stream of the first long-name entry, or of the short
    /// entry when it has none.
    pub first_slot: usize,
    /// Offset of the short entry.
    pub slot: usize,
    pub raw: [u8; 32],
    pub long: Option<String>,
}

impl Entry {
    pub fn attributes(&self) -> u8 {
        self.raw[11]
    }

    pub fn is_dir(&self) -> bool {
        self.raw[11] & 0x10 != 0
    }

    pub fn size(&self) -> u32 {
        u32::from_le_bytes(self.raw[28..32].try_into().expect("four bytes"))
    }

    pub fn first_cluster(&self, kind: FatKind) -> u32 {
        let high = if kind == FatKind::Fat32 {
            u32::from(u16::from_le_bytes([self.raw[20], self.raw[21]])) & 0x0FFF
        } else {
            0
        };
        (high << 16) | u32::from(u16::from_le_bytes([self.raw[26], self.raw[27]]))
    }

    pub fn short_display(&self) -> String {
        short_name(&self.raw, false)
    }

    /// The name to show: the long name when there is one.
    pub fn display(&self) -> String {
        self.long.clone().unwrap_or_else(|| self.short_display())
    }

    pub fn is_dot(&self) -> bool {
        self.raw[0] == b'.' && (self.raw[1] == b' ' || (self.raw[1] == b'.' && self.raw[2] == b' '))
    }

    pub fn matches(&self, name: &str) -> bool {
        !self.is_dot()
            && (self
                .long
                .as_deref()
                .is_some_and(|long| names::same_name(long, name))
                || names::same_name(&self.short_display(), name))
    }
}

/// Room for entries: where, and how many clusters the directory must grow.
pub(crate) struct Placement {
    pub offset: usize,
    pub extend: u32,
}

const LFN_POSITIONS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];

/// The live entries of a directory stream, up to its end marker.
pub(crate) fn parse(bytes: &[u8]) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut words: Vec<u16> = Vec::new();
    let mut expected = 0u8;
    let mut check = 0u8;
    let mut first_slot = 0usize;
    for (index, chunk) in bytes.chunks_exact(32).enumerate() {
        let raw: [u8; 32] = chunk.try_into().expect("32 bytes");
        let offset = index * 32;
        if raw[0] == 0 {
            break;
        }
        if raw[0] == 0xE5 {
            expected = 0;
            continue;
        }
        if raw[11] == 0x0F {
            let sequence = raw[0] & 0x1F;
            if sequence == 0 || sequence > 20 || raw[12] != 0 || raw[26] != 0 || raw[27] != 0 {
                expected = 0;
                continue;
            }
            if raw[0] & 0x40 != 0 {
                words = vec![0xFFFF; usize::from(sequence) * 13];
                expected = sequence;
                check = raw[13];
                first_slot = offset;
            }
            if expected == 0 || sequence != expected || raw[13] != check {
                expected = 0;
                continue;
            }
            let base = usize::from(sequence - 1) * 13;
            for (slot, position) in LFN_POSITIONS.iter().enumerate() {
                words[base + slot] = u16::from_le_bytes([raw[*position], raw[*position + 1]]);
            }
            expected -= 1;
            continue;
        }
        let has_long = expected == 0 && !words.is_empty() && check == short_checksum(&raw[..11]);
        let long = has_long
            .then(|| {
                let end = words.iter().position(|w| *w == 0).unwrap_or(words.len());
                String::from_utf16(&words[..end])
                    .ok()
                    .filter(|name| !name.is_empty())
            })
            .flatten();
        let start = if long.is_some() { first_slot } else { offset };
        words.clear();
        expected = 0;
        if raw[11] & 0x08 != 0 {
            continue;
        }
        entries.push(Entry {
            first_slot: start,
            slot: offset,
            raw,
            long,
        });
    }
    entries
}

impl<Device: BlockDevice> WriteVolume<Device> {
    pub(crate) fn root_id(&self) -> DirId {
        match self.volume.boot().kind {
            FatKind::Fat32 => DirId(self.volume.boot().root_cluster),
            _ => DirId(0),
        }
    }

    pub(crate) fn cluster_bytes(&self) -> usize {
        self.volume.boot().cluster_size() as usize
    }

    fn fixed_root_offset(&self) -> u64 {
        let boot = self.volume.boot();
        (u64::from(boot.reserved_sectors)
            + u64::from(boot.fat_count) * u64::from(boot.sectors_per_fat))
            * u64::from(boot.bytes_per_sector)
    }

    pub(crate) fn load_dir(&self, id: DirId) -> Result<DirStream> {
        if id.0 == 0 {
            let boot = self.volume.boot();
            let length = u64::from(boot.root_dir_sectors) * u64::from(boot.bytes_per_sector);
            let mut bytes = vec![0u8; length as usize];
            self.volume
                .read_bytes(self.fixed_root_offset(), &mut bytes)?;
            return Ok(DirStream {
                id,
                bytes,
                clusters: Vec::new(),
            });
        }
        let clusters = self.chain(id.0)?;
        let size = self.cluster_bytes();
        let mut bytes = vec![0u8; clusters.len() * size];
        for (index, cluster) in clusters.iter().enumerate() {
            let offset = self.volume.cluster_offset(*cluster)?;
            self.volume
                .read_bytes(offset, &mut bytes[index * size..(index + 1) * size])?;
        }
        Ok(DirStream {
            id,
            bytes,
            clusters,
        })
    }

    /// Writes `data` at `offset` in the directory stream, in pieces that stay
    /// inside one cluster each. The stream's copy is updated too.
    pub(crate) fn write_dir_bytes(
        &mut self,
        stream: &mut DirStream,
        offset: usize,
        data: &[u8],
    ) -> Result<()> {
        if offset + data.len() > stream.bytes.len() {
            return Err(Error::Bounds);
        }
        if stream.id.0 == 0 {
            let base = self.fixed_root_offset();
            self.write_volume(base + offset as u64, data)?;
        } else {
            let size = self.cluster_bytes();
            let mut done = 0;
            while done < data.len() {
                let at = offset + done;
                let within = at % size;
                let piece = (size - within).min(data.len() - done);
                let cluster = stream.clusters[at / size];
                let device = self.volume.cluster_offset(cluster)? + within as u64;
                self.write_volume(device, &data[done..done + piece])?;
                done += piece;
            }
        }
        stream.bytes[offset..offset + data.len()].copy_from_slice(data);
        Ok(())
    }

    pub(crate) fn find_entry(&self, stream: &DirStream, name: &str) -> Option<Entry> {
        parse(&stream.bytes)
            .into_iter()
            .find(|entry| entry.matches(name))
    }

    pub(crate) fn short_taken(
        &self,
        stream: &DirStream,
        short: &[u8; 11],
        except: Option<usize>,
    ) -> bool {
        parse(&stream.bytes)
            .iter()
            .any(|entry| Some(entry.slot) != except && entry.raw[..11] == short[..])
    }

    /// Finds room for `count` consecutive entries, or says how far the
    /// directory has to grow, or that it cannot.
    pub(crate) fn plan_insert(&self, stream: &DirStream, count: usize) -> Result<Placement> {
        let total = stream.bytes.len() / 32;
        let mut run = 0usize;
        let mut start = 0usize;
        for index in 0..total {
            let first = stream.bytes[index * 32];
            if first == 0 || first == 0xE5 {
                if run == 0 {
                    start = index;
                }
                run += 1;
                if run == count {
                    return Ok(Placement {
                        offset: start * 32,
                        extend: 0,
                    });
                }
            } else {
                run = 0;
            }
        }
        if stream.id.0 == 0 {
            return Err(Error::RootDirectoryFull);
        }
        let per_cluster = self.cluster_bytes() / 32;
        let extend = (count - run).div_ceil(per_cluster);
        if total + extend * per_cluster > MAX_ENTRIES {
            return Err(Error::DirectoryFull);
        }
        Ok(Placement {
            offset: (total - run) * 32,
            extend: extend as u32,
        })
    }

    /// Writes `entries` where [`plan_insert`](Self::plan_insert) said, growing
    /// the directory first if it asked to. The caller has checked there are
    /// enough free clusters. New clusters are zeroed before they are linked.
    pub(crate) fn insert_entries(
        &mut self,
        stream: &mut DirStream,
        placement: &Placement,
        entries: &[[u8; 32]],
    ) -> Result<()> {
        if placement.extend > 0 {
            let new = self.allocate(placement.extend)?;
            let zeros = vec![0u8; self.cluster_bytes()];
            for cluster in &new {
                let offset = self.volume.cluster_offset(*cluster)?;
                self.write_volume(offset, &zeros)?;
            }
            let last = stream.clusters.last().copied();
            self.link_chain(last, &new)?;
            stream
                .bytes
                .resize(stream.bytes.len() + new.len() * zeros.len(), 0);
            stream.clusters.extend(new);
        }
        let data: Vec<u8> = entries.iter().flatten().copied().collect();
        self.write_dir_bytes(stream, placement.offset, &data)
    }

    /// Marks an entry and its long-name entries deleted.
    pub(crate) fn delete_entry(&mut self, stream: &mut DirStream, entry: &Entry) -> Result<()> {
        let end = entry.slot + 32;
        let mut data = stream.bytes[entry.first_slot..end].to_vec();
        for chunk in data.chunks_exact_mut(32) {
            chunk[0] = 0xE5;
        }
        self.write_dir_bytes(stream, entry.first_slot, &data)
    }
}
