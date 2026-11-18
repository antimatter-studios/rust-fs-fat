//! The write side of the driver.
//!
//! Everything that can change a volume lives in this module and needs a
//! [`fs_core::BlockDevice`]. The readers in [`crate::volume`], [`crate::file`]
//! and [`crate::primitives`] take only `BlockRead`, so they cannot write by
//! type, and the constructors that open a source for reading (`open_device`,
//! `open_raw_device`, `open_path`) wrap what they are given in a
//! `ReadOnlyDevice`: a device that could write is still never written
//! through them. A writer is reached only through the explicit constructors
//! here, [`open_writable`], [`open_writable_path`], [`WriteVolume::mount`] and
//! [`open_raw_writable`].
//!
//! # Exclusive access
//!
//! A [`WriteVolume`] assumes it is the only thing using the volume. It reads
//! the FAT and directories from the device on demand and keeps no view of
//! them that another writer could invalidate, but it cannot tell that
//! another writer (a kernel mount, another process) changed the volume, and
//! two writers corrupt it. The caller keeps exclusive access for the life of
//! the mount.
//!
//! # Dirty and clean
//!
//! FAT16 and FAT32 record whether the volume was shut down cleanly in two
//! places: the boot sector's state byte (bit 0 dirty, bit 1 disk error) and
//! the top bits of FAT entry 1 (a clean-shutdown bit and a no-hard-error bit).
//! A writer refuses a volume where either says dirty or in error, because
//! only an implementation that checks and repairs the volume may clear those.
//! Before it changes anything the mount sets both to dirty and flushes, and a
//! clean [`WriteVolume::unmount`] flushes everything and clears them last, so
//! a crash leaves the volume marked dirty. A mount that changed nothing writes
//! nothing. FAT12 has no such flags, so the mount cannot mark or check one.
//!
//! # Failure
//!
//! A write that fails after it has already written something leaves the
//! volume possibly inconsistent. The mount then stays marked dirty and takes
//! no more writes; [`WriteVolume::unmount`] reports it.
//!
//! # Bounds and flushing
//!
//! Every write is checked against the volume's own length, and a write that
//! does not cover whole sectors is read, patched and written back as whole
//! sectors, so the device only ever sees sector-aligned transfers. Writes go
//! to the device as they are made; [`WriteVolume::flush`] (and unmount) ask the
//! device to put them on stable storage.

use crate::boot::FatKind;
use crate::primitives::PrimitiveAPI;
use crate::timestamp::FatTimestamp;
use crate::volume::{Volume, VolumeAPI};
use crate::{Error, Result};
use fs_core::{BlockDevice, FileDevice, OwnedRwSlice};
use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

mod alloc;
mod dir;
mod names;
mod ops;

pub use ops::{EntryInfo, FileHandle};

/// Gives each mount an identity its handles carry.
static NEXT_VOLUME: AtomicU64 = AtomicU64::new(1);

/// Boot-sector state byte: the volume was not shut down cleanly.
const STATE_DIRTY: u8 = 0x01;
/// Boot-sector state byte: the volume has a disk error.
const STATE_ERROR: u8 = 0x02;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Mounted,
    /// An operation failed after it had written something.
    Failed,
    Closed,
}

/// A volume mounted for writing.
///
/// Reads go through [`Deref`] to the strict [`Volume`] over the same device,
/// which sees every write as soon as it is made.
pub struct WriteVolume<Device: BlockDevice> {
    volume: Volume<Device>,
    state: State,
    marked_dirty: bool,
    writes: u64,
    /// The FAT copies a write keeps in step: all of them, or only the active
    /// one when a FAT32 volume has mirroring turned off.
    fat_copies: Vec<u8>,
    /// This mount's identity, carried by its handles.
    id: u64,
    /// Free clusters, counted at mount and kept current.
    free: u32,
    /// Where the next allocation starts looking.
    next_free: u32,
    /// The FAT32 FSInfo sector, when the volume has a valid one.
    fsinfo: Option<alloc::FsInfo>,
    /// The identity of each directory entry a handle was made for, by
    /// (directory, slot). An entry that is removed or replaced loses its
    /// identity, which is what makes handles to it stale.
    slots: HashMap<(u32, u32), u64>,
    next_identity: u64,
    clock: fn() -> FatTimestamp,
}

impl<Device: BlockDevice> Deref for WriteVolume<Device> {
    type Target = Volume<Device>;

    fn deref(&self) -> &Volume<Device> {
        &self.volume
    }
}

impl<Device: BlockDevice> WriteVolume<Device> {
    /// Mounts the volume at the start of `device` for writing.
    ///
    /// The device must be writable. The boot sector and geometry are checked
    /// as for reading, and a volume that is marked dirty or in error is
    /// refused with [`Error::Dirty`] rather than written to. Nothing is
    /// written until the first change.
    pub fn mount(device: Device) -> Result<Self> {
        if !device.is_writable() {
            return Err(Error::Device(fs_core::Error::ReadOnly));
        }
        let volume = Volume::open(device)?;
        let boot = volume.boot().clone();
        let mut sector = [0u8; 512];
        volume.read_volume_bytes(0, &mut sector)?;
        let mirroring_off = boot.kind == FatKind::Fat32 && sector[40] & 0x80 != 0;
        let fat_copies = if mirroring_off {
            vec![boot.active_fat]
        } else {
            (0..boot.fat_count).collect()
        };
        if let Some(offset) = state_offset(boot.kind) {
            let state = sector[offset];
            if state & STATE_DIRTY != 0 {
                return Err(Error::Dirty(
                    "the boot sector marks the volume dirty; check and repair it before writing",
                ));
            }
            if state & STATE_ERROR != 0 {
                return Err(Error::Dirty(
                    "the boot sector reports a disk error; check and repair it before writing",
                ));
            }
            let (clean, no_error) = entry_one_masks(boot.kind);
            let entry = volume.fat_entry_raw(1)?;
            if entry & clean == 0 {
                return Err(Error::Dirty(
                    "FAT entry 1 says the volume was not shut down cleanly; check and repair it before writing",
                ));
            }
            if entry & no_error == 0 {
                return Err(Error::Dirty(
                    "FAT entry 1 reports a hard error; check and repair it before writing",
                ));
            }
        }
        let fsinfo = if boot.kind == FatKind::Fat32 {
            fsinfo_state(&volume, &sector, u64::from(boot.bytes_per_sector))?
        } else {
            None
        };
        let mut mount = Self {
            volume,
            state: State::Mounted,
            marked_dirty: false,
            writes: 0,
            fat_copies,
            id: NEXT_VOLUME.fetch_add(1, Ordering::Relaxed),
            free: 0,
            next_free: 2,
            fsinfo,
            slots: HashMap::new(),
            next_identity: 0,
            clock: FatTimestamp::now,
        };
        mount.free = mount.scan_free()?;
        if let Some(info) = &mount.fsinfo {
            let hint = info.written_next;
            if (2..boot.cluster_count + 2).contains(&hint) {
                mount.next_free = hint;
            }
        }
        Ok(mount)
    }

    /// Asks the device to put every write made so far on stable storage.
    pub fn flush(&mut self) -> Result<()> {
        self.writable()?;
        self.volume.device().flush()?;
        Ok(())
    }

    /// Flushes the device and marks the volume clean.
    ///
    /// A mount on which an operation failed partway is flushed but left
    /// marked dirty, and this reports [`Error::Failed`].
    pub fn unmount(mut self) -> Result<()> {
        self.finish()
    }

    /// Whether the volume is currently marked dirty by this mount.
    pub fn is_marked_dirty(&self) -> bool {
        self.marked_dirty
    }

    /// Writes the FAT entry for `cluster` into every FAT copy the mount keeps
    /// in step. This is a primitive: it does not allocate or check the chain.
    /// Clusters 0 and 1 are the reserved entries and may be written.
    pub fn write_fat_entry(&mut self, cluster: u32, value: u32) -> Result<()> {
        let largest = match self.volume.boot().kind {
            FatKind::Fat12 => 0x0FFF,
            FatKind::Fat16 => 0xFFFF,
            FatKind::Fat32 => 0x0FFF_FFFF,
        };
        if value > largest {
            return Err(Error::Invalid(
                "FAT entry value is too large for the variant",
            ));
        }
        if cluster >= self.volume.boot().cluster_count + 2 {
            return Err(Error::Bounds);
        }
        self.mutate(|volume| {
            let old = volume.fat_entries(cluster, 1)?[0];
            volume.write_fat_entries(&[(cluster, value)])?;
            if cluster >= 2 {
                if old == 0 && value != 0 {
                    volume.free = volume.free.saturating_sub(1);
                } else if old != 0 && value == 0 {
                    volume.free += 1;
                }
                volume.write_fsinfo()?;
            }
            Ok(())
        })
    }

    /// Writes `data` at `offset` within a data cluster.
    pub fn write_cluster(&mut self, cluster: u32, offset: u64, data: &[u8]) -> Result<()> {
        let end = offset.checked_add(data.len() as u64).ok_or(Error::Bounds)?;
        if end > self.volume.boot().cluster_size() {
            return Err(Error::Bounds);
        }
        let start = self
            .volume
            .cluster_offset(cluster)?
            .checked_add(offset)
            .ok_or(Error::Bounds)?;
        self.mutate(|volume| volume.write_volume(start, data))
    }

    fn writable(&self) -> Result<()> {
        match self.state {
            State::Mounted => Ok(()),
            State::Failed => Err(Error::Failed),
            State::Closed => Err(Error::Closed),
        }
    }

    /// Runs one change: marks the volume dirty first, and if the change
    /// fails after writing anything, leaves the mount failed and dirty.
    pub(crate) fn mutate<Value>(
        &mut self,
        body: impl FnOnce(&mut Self) -> Result<Value>,
    ) -> Result<Value> {
        self.writable()?;
        self.mark_dirty()?;
        let before = self.writes;
        let result = body(self);
        if result.is_err() && self.writes != before {
            self.state = State::Failed;
            let _ = self.volume.device().flush();
        }
        result
    }

    fn mark_dirty(&mut self) -> Result<()> {
        if self.marked_dirty {
            return Ok(());
        }
        self.write_clean_flags(false)?;
        self.volume.device().flush()?;
        self.marked_dirty = true;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        match self.state {
            State::Closed => Ok(()),
            State::Failed => {
                self.state = State::Closed;
                let _ = self.volume.device().flush();
                Err(Error::Failed)
            }
            State::Mounted => {
                self.volume.device().flush()?;
                if self.marked_dirty {
                    self.write_clean_flags(true)?;
                    self.volume.device().flush()?;
                    self.marked_dirty = false;
                }
                self.state = State::Closed;
                Ok(())
            }
        }
    }

    /// Sets or clears the dirty marks: the boot sector's state byte and FAT
    /// entry 1, where the variant has them.
    fn write_clean_flags(&mut self, clean: bool) -> Result<()> {
        let kind = self.volume.boot().kind;
        let Some(offset) = state_offset(kind) else {
            return Ok(());
        };
        let mut state = [0u8];
        self.volume.read_volume_bytes(offset as u64, &mut state)?;
        if clean {
            state[0] &= !STATE_DIRTY;
        } else {
            state[0] |= STATE_DIRTY;
        }
        self.write_volume(offset as u64, &state)?;
        let (clean_mask, _) = entry_one_masks(kind);
        let entry = self.volume.fat_entry_raw(1)?;
        let entry = if clean {
            entry | clean_mask
        } else {
            entry & !clean_mask
        };
        self.write_fat_entries(&[(1, entry)])
    }

    /// Writes `data` at a byte offset within the volume. The range must lie
    /// inside the volume. The device sees whole sectors only.
    pub(crate) fn write_volume(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let end = offset.checked_add(data.len() as u64).ok_or(Error::Bounds)?;
        if end > self.volume.size_bytes() {
            return Err(Error::Bounds);
        }
        let sector = u64::from(self.volume.boot().bytes_per_sector);
        let first = offset / sector * sector;
        let last = end.div_ceil(sector) * sector;
        self.writes += 1;
        if first == offset && last == end {
            self.volume.device().write_at(offset, data)?;
            return Ok(());
        }
        let mut buffer = vec![0u8; (last - first) as usize];
        self.volume.device().read_at(first, &mut buffer)?;
        let start = (offset - first) as usize;
        buffer[start..start + data.len()].copy_from_slice(data);
        self.volume.device().write_at(first, &buffer)?;
        Ok(())
    }

    /// Writes FAT entries into every copy the mount keeps in step. Entries
    /// that share a sector are written together, and FAT12 entries that
    /// straddle a sector boundary are patched across both sectors. The reserved
    /// high four bits of a FAT32 entry are kept as they are.
    pub(crate) fn write_fat_entries(&mut self, updates: &[(u32, u32)]) -> Result<()> {
        let boot = self.volume.boot().clone();
        let sector = u64::from(boot.bytes_per_sector);
        let fat_bytes = u64::from(boot.sectors_per_fat) * sector;
        for &(cluster, _) in updates {
            if cluster >= boot.cluster_count + 2 {
                return Err(Error::Bounds);
            }
        }
        for copy in self.fat_copies.clone() {
            let base = (u64::from(boot.reserved_sectors)
                + u64::from(copy) * u64::from(boot.sectors_per_fat))
                * sector;
            let mut sectors: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
            for &(cluster, value) in updates {
                let (offset, length) = match boot.kind {
                    FatKind::Fat12 => (u64::from(cluster) * 3 / 2, 2),
                    FatKind::Fat16 => (u64::from(cluster) * 2, 2),
                    FatKind::Fat32 => (u64::from(cluster) * 4, 4),
                };
                if offset + length > fat_bytes {
                    return Err(Error::Bounds);
                }
                for index in offset / sector..=(offset + length - 1) / sector {
                    if let std::collections::btree_map::Entry::Vacant(slot) = sectors.entry(index) {
                        let mut bytes = vec![0u8; sector as usize];
                        self.volume
                            .read_volume_bytes(base + index * sector, &mut bytes)?;
                        slot.insert(bytes);
                    }
                }
                let mut entry = [0u8; 4];
                for (position, byte) in entry.iter_mut().enumerate().take(length as usize) {
                    let at = offset + position as u64;
                    *byte = sectors[&(at / sector)][(at % sector) as usize];
                }
                match boot.kind {
                    FatKind::Fat12 => {
                        let word = u16::from_le_bytes([entry[0], entry[1]]);
                        let word = if cluster & 1 == 0 {
                            (word & 0xF000) | (value as u16 & 0x0FFF)
                        } else {
                            (word & 0x000F) | ((value as u16 & 0x0FFF) << 4)
                        };
                        entry[..2].copy_from_slice(&word.to_le_bytes());
                    }
                    FatKind::Fat16 => entry[..2].copy_from_slice(&(value as u16).to_le_bytes()),
                    FatKind::Fat32 => {
                        let word = u32::from_le_bytes(entry);
                        entry = ((word & 0xF000_0000) | (value & 0x0FFF_FFFF)).to_le_bytes();
                    }
                }
                for (position, byte) in entry.iter().enumerate().take(length as usize) {
                    let at = offset + position as u64;
                    sectors
                        .get_mut(&(at / sector))
                        .expect("sector loaded above")[(at % sector) as usize] = *byte;
                }
            }
            // Adjacent sectors go out as one write.
            let mut pending: Option<(u64, Vec<u8>)> = None;
            for (index, bytes) in sectors {
                match pending.as_mut() {
                    Some((start, run)) if *start + run.len() as u64 == index * sector => {
                        run.extend_from_slice(&bytes);
                    }
                    _ => {
                        if let Some((start, run)) = pending.take() {
                            self.write_volume(base + start, &run)?;
                        }
                        pending = Some((index * sector, bytes));
                    }
                }
            }
            if let Some((start, run)) = pending {
                self.write_volume(base + start, &run)?;
            }
        }
        Ok(())
    }
}

impl<Device: BlockDevice> Drop for WriteVolume<Device> {
    /// A mount dropped without [`WriteVolume::unmount`] is closed here, and
    /// any error is lost; call `unmount` to see it. A mount dropped while its
    /// thread panics is flushed but left marked dirty, because the panic may
    /// have stopped a change partway.
    fn drop(&mut self) {
        if std::thread::panicking() && self.state == State::Mounted {
            self.state = State::Failed;
        }
        let _ = self.finish();
    }
}

/// The FSInfo sector, if the volume has one whose signatures are right, with
/// the hint it holds. A damaged or missing FSInfo is left alone.
fn fsinfo_state<Device: BlockDevice>(
    volume: &Volume<Device>,
    boot: &[u8; 512],
    sector: u64,
) -> Result<Option<alloc::FsInfo>> {
    let number = u64::from(u16::from_le_bytes([boot[48], boot[49]]));
    if number == 0 || number >= u64::from(volume.boot().reserved_sectors) {
        return Ok(None);
    }
    let mut info = [0u8; 512];
    volume.read_volume_bytes(number * sector, &mut info)?;
    let word = |at: usize| u32::from_le_bytes([info[at], info[at + 1], info[at + 2], info[at + 3]]);
    if word(0) != 0x4161_5252 || word(484) != 0x6141_7272 || word(508) != 0xAA55_0000 {
        return Ok(None);
    }
    Ok(Some(alloc::FsInfo {
        offset: number * sector,
        written_free: word(488),
        written_next: word(492),
    }))
}

/// The offset of the boot sector's state byte, for the variants that have one.
fn state_offset(kind: FatKind) -> Option<usize> {
    match kind {
        FatKind::Fat12 => None,
        FatKind::Fat16 => Some(37),
        FatKind::Fat32 => Some(65),
    }
}

/// The clean-shutdown and no-hard-error bits of FAT entry 1.
fn entry_one_masks(kind: FatKind) -> (u32, u32) {
    match kind {
        FatKind::Fat12 | FatKind::Fat16 => (0x8000, 0x4000),
        FatKind::Fat32 => (0x0800_0000, 0x0400_0000),
    }
}

/// Mounts the volume at a 512-byte-aligned `offset` within `device` for
/// writing. This and [`open_writable_path`] are the only ways to a writable
/// volume besides [`WriteVolume::mount`].
pub fn open_writable<Device: BlockDevice + 'static>(
    device: Device,
    offset: u64,
) -> Result<WriteVolume<OwnedRwSlice>> {
    if !offset.is_multiple_of(512) || offset >= device.size_bytes() {
        return Err(Error::Invalid(
            "volume offset must be aligned and within device",
        ));
    }
    let length = device.size_bytes() - offset;
    let device: Arc<dyn BlockDevice> = Arc::new(device);
    WriteVolume::mount(OwnedRwSlice::new(device, offset, length))
}

/// Opens an image or device file read-write and mounts the volume at `offset`
/// within it for writing.
pub fn open_writable_path(path: &Path, offset: u64) -> Result<WriteVolume<OwnedRwSlice>> {
    open_writable(FileDevice::open_rw(path)?, offset)
}

/// Low-level byte writing over a volume, with no filesystem behind it.
///
/// This is **not** a safe mount. It does not read the boot sector, does not
/// check or set the dirty flags, and writes any bytes it is given anywhere in
/// the range it was opened on, including over the FAT and the boot sector. It
/// exists for tools that repair or build a volume byte by byte. A volume
/// written this way is the caller's to keep consistent. Use [`WriteVolume`]
/// to change files and directories.
pub struct RawWriteVolume<Device: BlockDevice> {
    device: Device,
}

impl<Device: BlockDevice> RawWriteVolume<Device> {
    /// Opens `device` for raw byte writing. The device must be writable.
    pub fn open(device: Device) -> Result<Self> {
        if !device.is_writable() {
            return Err(Error::Device(fs_core::Error::ReadOnly));
        }
        Ok(Self { device })
    }

    /// Writes `data` at `offset`, bounded to the device.
    pub fn write_volume_bytes(&self, offset: u64, data: &[u8]) -> Result<()> {
        let end = offset.checked_add(data.len() as u64).ok_or(Error::Bounds)?;
        if end > self.device.size_bytes() {
            return Err(Error::Bounds);
        }
        self.device.write_at(offset, data)?;
        Ok(())
    }

    /// Asks the device to put every write on stable storage.
    pub fn flush(&self) -> Result<()> {
        self.device.flush()?;
        Ok(())
    }
}

impl<Device: BlockDevice> VolumeAPI for RawWriteVolume<Device> {
    fn size_bytes(&self) -> u64 {
        self.device.size_bytes()
    }

    fn read_volume_bytes(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(output.len() as u64)
            .ok_or(Error::Bounds)?;
        if end > self.device.size_bytes() {
            return Err(Error::Bounds);
        }
        self.device.read_at(offset, output)?;
        Ok(())
    }
}

/// Opens `device` at a 512-byte-aligned `offset` for raw byte writing.
pub fn open_raw_writable<Device: BlockDevice + 'static>(
    device: Device,
    offset: u64,
) -> Result<RawWriteVolume<OwnedRwSlice>> {
    if !offset.is_multiple_of(512) || offset >= device.size_bytes() {
        return Err(Error::Invalid(
            "volume offset must be aligned and within device",
        ));
    }
    let length = device.size_bytes() - offset;
    let device: Arc<dyn BlockDevice> = Arc::new(device);
    RawWriteVolume::open(OwnedRwSlice::new(device, offset, length))
}
