//! The file operations: create, mkdir, write, truncate, rename, unlink,
//! rmdir, and the reads that go with them.
//!
//! Paths use `/` between names and are resolved from the root; names compare
//! without regard to case. Each operation runs in the order that leaves the
//! volume consistent if it stops partway: growing writes the FAT, then the
//! data, then the directory entry that refers to them; shrinking and removing
//! write the directory entry first and free the clusters after. Before an
//! operation writes anything it checks that the volume has the room it needs,
//! so a full volume or a full fixed root directory is refused with nothing
//! changed.

use super::dir::{self, DirId, DirStream, Entry};
use super::names;
use super::WriteVolume;
use crate::boot::FatKind;
use crate::timestamp::FatTimestamp;
use crate::volume::VolumeAPI;
use crate::{Error, Result};
use fs_core::BlockDevice;

const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;

/// What a directory holds about one file or directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryInfo {
    /// The long name when there is one, else the short name.
    pub name: String,
    /// The 8.3 alias as stored, e.g. `README~1.TXT`.
    pub short_name: String,
    pub is_dir: bool,
    pub attributes: u8,
    pub size: u32,
    pub first_cluster: u32,
    pub created: FatTimestamp,
    pub modified: FatTimestamp,
    /// FAT date of the last access (the field has no time).
    pub accessed_date: u16,
}

/// An open file. It belongs to the volume that made it and to the directory
/// entry it was opened on: after the file is removed, replaced or renamed,
/// every use of the handle fails with [`Error::StaleHandle`] rather than
/// reaching whatever now sits in that slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileHandle {
    volume: u64,
    dir: u32,
    slot: u32,
    identity: u64,
    short: [u8; 11],
}

struct Located {
    parent: DirStream,
    /// Every directory from the root down to and including the parent.
    path: Vec<DirId>,
    leaf: String,
    entry: Option<Entry>,
}

fn split(path: &str) -> Result<Vec<&str>> {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if parts.iter().any(|part| *part == "." || *part == "..") {
        return Err(Error::InvalidName("paths hold no '.' or '..' parts"));
    }
    Ok(parts)
}

fn info(kind: FatKind, entry: &Entry) -> EntryInfo {
    let raw = &entry.raw;
    let word = |at: usize| u16::from_le_bytes([raw[at], raw[at + 1]]);
    EntryInfo {
        name: entry.display(),
        short_name: entry.short_display(),
        is_dir: entry.is_dir(),
        attributes: entry.attributes(),
        size: entry.size(),
        first_cluster: entry.first_cluster(kind),
        created: FatTimestamp {
            date: word(16),
            time: word(14),
            tenths: raw[13],
        },
        modified: FatTimestamp {
            date: word(24),
            time: word(22),
            tenths: 0,
        },
        accessed_date: word(18),
    }
}

fn short_entry(
    short: &[u8; 11],
    nt_flags: u8,
    attributes: u8,
    first_cluster: u32,
    size: u32,
    kind: FatKind,
    now: FatTimestamp,
) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[..11].copy_from_slice(short);
    entry[11] = attributes;
    entry[12] = nt_flags;
    entry[13] = now.tenths;
    entry[14..16].copy_from_slice(&now.time.to_le_bytes());
    entry[16..18].copy_from_slice(&now.date.to_le_bytes());
    entry[18..20].copy_from_slice(&now.date.to_le_bytes());
    if kind == FatKind::Fat32 {
        entry[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    }
    entry[22..24].copy_from_slice(&now.time.to_le_bytes());
    entry[24..26].copy_from_slice(&now.date.to_le_bytes());
    entry[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    entry
}

impl<Device: BlockDevice> WriteVolume<Device> {
    /// Sets the clock that stamps new and changed entries. The default reads
    /// the system clock as UTC.
    pub fn set_clock(&mut self, clock: fn() -> FatTimestamp) {
        self.clock = clock;
    }

    /// Free clusters, as counted at mount and kept current.
    pub fn free_clusters(&self) -> u32 {
        self.free
    }

    /// Runs one operation: marks the volume dirty first, brings the FAT32
    /// FSInfo hints up to date at the end, and fails the mount if the
    /// operation fails after it wrote something.
    fn transact<Value>(&mut self, body: impl FnOnce(&mut Self) -> Result<Value>) -> Result<Value> {
        self.mutate(|volume| {
            let value = body(volume)?;
            volume.write_fsinfo()?;
            Ok(value)
        })
    }

    // ------------------------------------------------------------ lookup

    fn walk(&self, parts: &[&str]) -> Result<(DirStream, Vec<DirId>)> {
        let mut id = self.root_id();
        let mut path = vec![id];
        let mut stream = self.load_dir(id)?;
        for part in parts {
            let entry = self.find_entry(&stream, part).ok_or(Error::NotFound)?;
            if !entry.is_dir() {
                return Err(Error::NotADirectory);
            }
            let first = entry.first_cluster(self.volume.boot().kind);
            if first < 2 {
                return Err(Error::Invalid("directory entry has no cluster"));
            }
            id = DirId(first);
            path.push(id);
            stream = self.load_dir(id)?;
        }
        Ok((stream, path))
    }

    fn locate(&self, path: &str) -> Result<Located> {
        let parts = split(path)?;
        let Some((leaf, parents)) = parts.split_last() else {
            return Err(Error::InvalidName("the root directory has no name"));
        };
        let (parent, path) = self.walk(parents)?;
        let entry = self.find_entry(&parent, leaf);
        Ok(Located {
            parent,
            path,
            leaf: (*leaf).to_string(),
            entry,
        })
    }

    fn handle_for(&mut self, dir: DirId, slot: usize, short: [u8; 11]) -> FileHandle {
        let key = (dir.0, slot as u32);
        let next = &mut self.next_identity;
        let identity = *self.slots.entry(key).or_insert_with(|| {
            *next += 1;
            *next
        });
        FileHandle {
            volume: self.id,
            dir: dir.0,
            slot: slot as u32,
            identity,
            short,
        }
    }

    /// Where the handle's short entry is on the device, and its bytes.
    fn handle_entry(&self, handle: &FileHandle) -> Result<(u64, [u8; 32])> {
        if handle.volume != self.id {
            return Err(Error::ForeignHandle);
        }
        if self.slots.get(&(handle.dir, handle.slot)) != Some(&handle.identity) {
            return Err(Error::StaleHandle);
        }
        let slot = handle.slot as usize;
        let size = self.cluster_bytes();
        let offset = if handle.dir == 0 {
            let boot = self.volume.boot();
            (u64::from(boot.reserved_sectors)
                + u64::from(boot.fat_count) * u64::from(boot.sectors_per_fat))
                * u64::from(boot.bytes_per_sector)
                + slot as u64
        } else {
            let chain = self.chain(handle.dir)?;
            let cluster = *chain.get(slot / size).ok_or(Error::StaleHandle)?;
            self.volume.cluster_offset(cluster)? + (slot % size) as u64
        };
        let mut raw = [0u8; 32];
        self.volume.read_volume_bytes(offset, &mut raw)?;
        if raw[0] == 0 || raw[0] == 0xE5 || raw[11] == 0x0F || raw[11] & ATTR_DIRECTORY != 0 {
            return Err(Error::StaleHandle);
        }
        if raw[..11] != handle.short {
            return Err(Error::StaleHandle);
        }
        Ok((offset, raw))
    }

    // ------------------------------------------------------------- reads

    /// Lists a directory (`""` or `"/"` is the root), without `.` and `..`.
    pub fn read_dir(&self, path: &str) -> Result<Vec<EntryInfo>> {
        let (stream, _) = self.walk(&split(path)?)?;
        let kind = self.volume.boot().kind;
        Ok(dir::parse(&stream.bytes)
            .iter()
            .filter(|entry| !entry.is_dot())
            .map(|entry| info(kind, entry))
            .collect())
    }

    /// What the directory holds about the file or directory at `path`.
    pub fn stat(&self, path: &str) -> Result<EntryInfo> {
        let located = self.locate(path)?;
        let entry = located.entry.ok_or(Error::NotFound)?;
        Ok(info(self.volume.boot().kind, &entry))
    }

    /// Opens the file at `path`.
    pub fn open(&mut self, path: &str) -> Result<FileHandle> {
        let located = self.locate(path)?;
        let entry = located.entry.ok_or(Error::NotFound)?;
        if entry.is_dir() {
            return Err(Error::IsADirectory);
        }
        Ok(self.handle_for(
            located.parent.id,
            entry.slot,
            entry.raw[..11].try_into().expect("11 bytes"),
        ))
    }

    /// The length of the file the handle names.
    pub fn len(&self, handle: &FileHandle) -> Result<u64> {
        let (_, raw) = self.handle_entry(handle)?;
        Ok(u64::from(u32::from_le_bytes(
            raw[28..32].try_into().expect("4 bytes"),
        )))
    }

    pub fn is_empty(&self, handle: &FileHandle) -> Result<bool> {
        Ok(self.len(handle)? == 0)
    }

    fn first_cluster_of(&self, raw: &[u8; 32]) -> u32 {
        let high = if self.volume.boot().kind == FatKind::Fat32 {
            u32::from(u16::from_le_bytes([raw[20], raw[21]])) & 0x0FFF
        } else {
            0
        };
        (high << 16) | u32::from(u16::from_le_bytes([raw[26], raw[27]]))
    }

    /// Reads from the file at `offset`, up to the end of the file. Returns the
    /// number of bytes read.
    pub fn read(&self, handle: &FileHandle, offset: u64, output: &mut [u8]) -> Result<usize> {
        let (_, raw) = self.handle_entry(handle)?;
        let size = u64::from(u32::from_le_bytes(raw[28..32].try_into().expect("4 bytes")));
        if offset >= size || output.is_empty() {
            return Ok(0);
        }
        let count = (size - offset).min(output.len() as u64) as usize;
        let chain = self.chain(self.first_cluster_of(&raw))?;
        let cluster_size = self.cluster_bytes() as u64;
        if (chain.len() as u64) * cluster_size < offset + count as u64 {
            return Err(Error::Invalid("file chain ends before file size"));
        }
        let mut done = 0usize;
        while done < count {
            let position = offset + done as u64;
            let index = (position / cluster_size) as usize;
            let within = position % cluster_size;
            // Read on across clusters that follow one another on the device.
            let mut run = 1;
            while index + run < chain.len()
                && chain[index + run] == chain[index + run - 1] + 1
                && ((run as u64) * cluster_size) - within < (count - done) as u64
            {
                run += 1;
            }
            let piece = ((run as u64) * cluster_size - within).min((count - done) as u64) as usize;
            self.volume.read_volume_bytes(
                self.volume.cluster_offset(chain[index])? + within,
                &mut output[done..done + piece],
            )?;
            done += piece;
        }
        Ok(count)
    }

    // ------------------------------------------------------------ create

    /// Creates an empty file. The name must not exist.
    pub fn create_file(&mut self, path: &str) -> Result<FileHandle> {
        let (dir, slot, short) = self.transact(|volume| volume.create_entry(path, false))?;
        Ok(self.handle_for(dir, slot, short))
    }

    /// Creates an empty directory. The name must not exist.
    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        self.transact(|volume| volume.create_entry(path, true).map(|_| ()))
    }

    fn create_entry(&mut self, path: &str, directory: bool) -> Result<(DirId, usize, [u8; 11])> {
        let located = self.locate(path)?;
        if located.entry.is_some() {
            return Err(Error::Exists);
        }
        let mut stream = located.parent;
        let name = names::make(&located.leaf, |short| {
            self.short_taken(&stream, short, None)
        })?;
        let kind = self.volume.boot().kind;
        let now = (self.clock)();
        let mut entries = match &name.long {
            Some(units) => names::long_entries(units, &name.short),
            None => Vec::new(),
        };
        let cluster = 0;
        entries.push(short_entry(
            &name.short,
            name.nt_flags,
            if directory {
                ATTR_DIRECTORY
            } else {
                ATTR_ARCHIVE
            },
            cluster,
            0,
            kind,
            now,
        ));
        let placement = self.plan_insert(&stream, entries.len())?;
        let needed = placement.extend + u32::from(directory);
        if needed > self.free {
            return Err(Error::VolumeFull);
        }
        let last = entries.len() - 1;
        if directory {
            let new = self.allocate(1)?[0];
            let parent = if stream.id == self.root_id() {
                0
            } else {
                stream.id.0
            };
            let mut content = vec![0u8; self.cluster_bytes()];
            content[..32].copy_from_slice(&short_entry(
                b".          ",
                0,
                ATTR_DIRECTORY,
                new,
                0,
                kind,
                now,
            ));
            content[32..64].copy_from_slice(&short_entry(
                b"..         ",
                0,
                ATTR_DIRECTORY,
                parent,
                0,
                kind,
                now,
            ));
            let offset = self.volume.cluster_offset(new)?;
            self.write_volume(offset, &content)?;
            self.link_chain(None, &[new])?;
            entries[last] = short_entry(
                &name.short,
                name.nt_flags,
                ATTR_DIRECTORY,
                new,
                0,
                kind,
                now,
            );
            self.slots.retain(|key, _| key.0 != new);
        }
        self.insert_entries(&mut stream, &placement, &entries)?;
        let slot = placement.offset + last * 32;
        self.slots.remove(&(stream.id.0, slot as u32));
        Ok((stream.id, slot, name.short))
    }

    // ------------------------------------------------------------- write

    /// Writes `data` at `offset`, growing the file (and zero-filling any gap)
    /// as needed. Returns the number of bytes written, which is all of them.
    pub fn write(&mut self, handle: &FileHandle, offset: u64, data: &[u8]) -> Result<usize> {
        if data.is_empty() {
            self.handle_entry(handle)?;
            return Ok(0);
        }
        self.transact(|volume| volume.write_range(handle, offset, data, 0))?;
        Ok(data.len())
    }

    /// Sets the file's length: shorter frees clusters, longer zero-fills.
    pub fn truncate(&mut self, handle: &FileHandle, length: u64) -> Result<()> {
        if length > u64::from(u32::MAX) {
            return Err(Error::FileTooLarge);
        }
        self.transact(|volume| {
            let (_, raw) = volume.handle_entry(handle)?;
            let size = u64::from(u32::from_le_bytes(raw[28..32].try_into().expect("4 bytes")));
            if length > size {
                volume.write_range(handle, length, &[], length)
            } else if length < size {
                volume.shrink_file(handle, length)
            } else {
                Ok(())
            }
        })
    }

    /// The core of `write` and of growing `truncate`: puts `data` at `offset`
    /// and makes the file at least `at_least` long.
    fn write_range(
        &mut self,
        handle: &FileHandle,
        offset: u64,
        data: &[u8],
        at_least: u64,
    ) -> Result<()> {
        let (entry_offset, mut raw) = self.handle_entry(handle)?;
        let cluster_size = self.cluster_bytes() as u64;
        let size = u64::from(u32::from_le_bytes(raw[28..32].try_into().expect("4 bytes")));
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(Error::FileTooLarge)?;
        let target = size.max(end).max(at_least);
        if target > u64::from(u32::MAX) {
            return Err(Error::FileTooLarge);
        }
        let first = self.first_cluster_of(&raw);
        let mut chain = self.chain(first)?;
        if (chain.len() as u64) * cluster_size < size {
            return Err(Error::Invalid("file chain ends before file size"));
        }
        let needed = target.div_ceil(cluster_size) as usize;
        let have = chain.len();
        let extra = needed.saturating_sub(have) as u32;
        if extra > self.free {
            return Err(Error::VolumeFull);
        }
        let new = self.allocate(extra)?;
        // Whatever of the new area the data does not cover reads as zero; so
        // does the gap between the old end and the write, in the old last
        // cluster too.
        if offset > size {
            let stop = offset.min(have as u64 * cluster_size);
            if size < stop {
                self.zero_range(&chain, size, stop)?;
            }
        }
        let zeros = vec![0u8; cluster_size as usize];
        for (index, cluster) in new.iter().enumerate() {
            let start = (have + index) as u64 * cluster_size;
            let covered = !data.is_empty() && offset <= start && end >= start + cluster_size;
            if !covered {
                let device = self.volume.cluster_offset(*cluster)?;
                self.write_volume(device, &zeros)?;
            }
        }
        self.link_chain(chain.last().copied(), &new)?;
        chain.extend(new);
        self.write_clusters(&chain, offset, data)?;
        let now = (self.clock)();
        if first == 0 {
            let cluster = chain.first().copied().unwrap_or(0);
            raw[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
            if self.volume.boot().kind == FatKind::Fat32 {
                raw[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
            }
        }
        raw[28..32].copy_from_slice(&(target as u32).to_le_bytes());
        raw[22..24].copy_from_slice(&now.time.to_le_bytes());
        raw[24..26].copy_from_slice(&now.date.to_le_bytes());
        raw[18..20].copy_from_slice(&now.date.to_le_bytes());
        raw[11] |= ATTR_ARCHIVE;
        self.write_volume(entry_offset, &raw)
    }

    fn shrink_file(&mut self, handle: &FileHandle, length: u64) -> Result<()> {
        let (entry_offset, mut raw) = self.handle_entry(handle)?;
        let cluster_size = self.cluster_bytes() as u64;
        let first = self.first_cluster_of(&raw);
        let chain = self.chain_prefix(first)?;
        let keep = length.div_ceil(cluster_size) as usize;
        let now = (self.clock)();
        raw[28..32].copy_from_slice(&(length as u32).to_le_bytes());
        if keep == 0 {
            raw[20..22].fill(0);
            raw[26..28].fill(0);
        }
        raw[22..24].copy_from_slice(&now.time.to_le_bytes());
        raw[24..26].copy_from_slice(&now.date.to_le_bytes());
        raw[18..20].copy_from_slice(&now.date.to_le_bytes());
        self.write_volume(entry_offset, &raw)?;
        if keep == 0 {
            self.free_clusters_of(&chain)
        } else {
            self.shrink_chain(&chain, keep)
        }
    }

    /// Writes zeros over `[from, to)` of a file held in `chain`.
    fn zero_range(&mut self, chain: &[u32], from: u64, to: u64) -> Result<()> {
        let zeros = vec![0u8; (to - from) as usize];
        self.write_clusters(chain, from, &zeros)
    }

    /// Writes `data` at file offset `offset` over the clusters of `chain`,
    /// one device write for each run of clusters that follow one another.
    fn write_clusters(&mut self, chain: &[u32], offset: u64, data: &[u8]) -> Result<()> {
        let cluster_size = self.cluster_bytes() as u64;
        let mut done = 0usize;
        while done < data.len() {
            let position = offset + done as u64;
            let index = (position / cluster_size) as usize;
            let within = position % cluster_size;
            let mut run = 1;
            while index + run < chain.len()
                && chain[index + run] == chain[index + run - 1] + 1
                && (run as u64 * cluster_size) - within < (data.len() - done) as u64
            {
                run += 1;
            }
            let piece =
                (run as u64 * cluster_size - within).min((data.len() - done) as u64) as usize;
            let device = self.volume.cluster_offset(chain[index])? + within;
            self.write_volume(device, &data[done..done + piece])?;
            done += piece;
        }
        Ok(())
    }

    // ----------------------------------------------------------- removal

    /// Removes a file. A directory is refused; see [`rmdir`](Self::rmdir).
    pub fn unlink(&mut self, path: &str) -> Result<()> {
        self.transact(|volume| {
            let located = volume.locate(path)?;
            let entry = located.entry.ok_or(Error::NotFound)?;
            if entry.is_dir() {
                return Err(Error::IsADirectory);
            }
            let first = entry.first_cluster(volume.volume.boot().kind);
            let chain = volume.chain_prefix(first)?;
            let mut stream = located.parent;
            volume.delete_entry(&mut stream, &entry)?;
            volume.slots.remove(&(stream.id.0, entry.slot as u32));
            volume.free_clusters_of(&chain)
        })
    }

    /// Removes an empty directory.
    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        self.transact(|volume| {
            let located = volume.locate(path)?;
            let entry = located.entry.ok_or(Error::NotFound)?;
            if !entry.is_dir() {
                return Err(Error::NotADirectory);
            }
            let first = entry.first_cluster(volume.volume.boot().kind);
            if first < 2 {
                return Err(Error::Invalid("directory entry has no cluster"));
            }
            let child = volume.load_dir(DirId(first))?;
            if dir::parse(&child.bytes).iter().any(|entry| !entry.is_dot()) {
                return Err(Error::NotEmpty);
            }
            let chain = volume.chain_prefix(first)?;
            let mut stream = located.parent;
            volume.delete_entry(&mut stream, &entry)?;
            volume.slots.remove(&(stream.id.0, entry.slot as u32));
            volume.slots.retain(|key, _| key.0 != first);
            volume.free_clusters_of(&chain)
        })
    }

    // ------------------------------------------------------------ rename

    /// Renames or moves a file or directory. The target must not exist, except
    /// that a name may be changed in case alone. Handles to the renamed file
    /// become stale.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.transact(|volume| volume.rename_entry(from, to))
    }

    fn rename_entry(&mut self, from: &str, to: &str) -> Result<()> {
        let source = self.locate(from)?;
        let entry = source.entry.clone().ok_or(Error::NotFound)?;
        let target = self.locate(to)?;
        let same_dir = source.parent.id == target.parent.id;
        if let Some(existing) = &target.entry {
            if !(same_dir && existing.slot == entry.slot) {
                return Err(Error::Exists);
            }
        }
        let kind = self.volume.boot().kind;
        if entry.is_dir() {
            let moved = DirId(entry.first_cluster(kind));
            if target.path.contains(&moved) {
                return Err(Error::Invalid("a directory cannot be moved into itself"));
            }
        }
        if same_dir && entry.display() == target.leaf {
            return Ok(());
        }
        let mut source_stream = source.parent;
        let mut target_stream = target.parent;
        let except = same_dir.then_some(entry.slot);
        let name = names::make(&target.leaf, |short| {
            self.short_taken(&target_stream, short, except)
        })?;
        let mut entries = match &name.long {
            Some(units) => names::long_entries(units, &name.short),
            None => Vec::new(),
        };
        let mut raw = entry.raw;
        raw[..11].copy_from_slice(&name.short);
        raw[12] = (raw[12] & !(names::NT_LOWER_BASE | names::NT_LOWER_EXTENSION)) | name.nt_flags;
        entries.push(raw);
        let placement = self.plan_insert(&target_stream, entries.len())?;
        if placement.extend > self.free {
            return Err(Error::VolumeFull);
        }
        self.insert_entries(&mut target_stream, &placement, &entries)?;
        let new_slot = placement.offset + (entries.len() - 1) * 32;
        self.slots.remove(&(target_stream.id.0, new_slot as u32));
        if entry.is_dir() && !same_dir {
            // The moved directory's `..` names its new parent.
            let parent = if target_stream.id == self.root_id() {
                0
            } else {
                target_stream.id.0
            };
            let offset = self.volume.cluster_offset(entry.first_cluster(kind))? + 32;
            let mut dotdot = [0u8; 32];
            self.volume.read_volume_bytes(offset, &mut dotdot)?;
            dotdot[26..28].copy_from_slice(&(parent as u16).to_le_bytes());
            if kind == FatKind::Fat32 {
                dotdot[20..22].copy_from_slice(&((parent >> 16) as u16).to_le_bytes());
            }
            self.write_volume(offset, &dotdot)?;
        }
        if same_dir {
            self.delete_entry(&mut target_stream, &entry)?;
        } else {
            self.delete_entry(&mut source_stream, &entry)?;
        }
        self.slots.remove(&(source_stream.id.0, entry.slot as u32));
        Ok(())
    }
}
