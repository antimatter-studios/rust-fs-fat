//! Shared helpers for the write tests: an in-memory block device that counts
//! what is done to it, and small FAT images built byte by byte.
#![allow(dead_code)]

pub mod check;

use fs_core::{BlockDevice, BlockRead, Error as CoreError, Result as CoreResult};
use fs_fat32::FatKind;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    bytes: Vec<u8>,
    writes: u64,
    flushes: u64,
    set_lens: u64,
    /// Reads still allowed before every read fails, for cancellation tests.
    read_budget: Option<u64>,
    /// Writes still allowed before every write fails.
    write_budget: Option<u64>,
    writable: bool,
    events: Vec<Event>,
}

/// What was done to a device, in order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Write { offset: u64, length: usize },
    Flush,
}

/// A shared in-memory device. Clones see the same bytes and counters.
#[derive(Clone)]
pub struct Disk(Arc<Mutex<State>>);

impl Disk {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Arc::new(Mutex::new(State {
            bytes,
            writable: true,
            ..State::default()
        })))
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().bytes.clone()
    }

    pub fn writes(&self) -> u64 {
        self.0.lock().unwrap().writes
    }

    pub fn flushes(&self) -> u64 {
        self.0.lock().unwrap().flushes
    }

    pub fn set_lens(&self) -> u64 {
        self.0.lock().unwrap().set_lens
    }

    /// Every attempt to change the device, counted whether or not it worked.
    pub fn mutations(&self) -> u64 {
        let state = self.0.lock().unwrap();
        state.writes + state.flushes + state.set_lens
    }

    pub fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().events.clone()
    }

    pub fn clear_events(&self) {
        self.0.lock().unwrap().events.clear();
    }

    pub fn fail_writes_after(&self, writes: u64) {
        self.0.lock().unwrap().write_budget = Some(writes);
    }

    pub fn fail_reads_after(&self, reads: u64) {
        self.0.lock().unwrap().read_budget = Some(reads);
    }

    pub fn set_writable(&self, writable: bool) {
        self.0.lock().unwrap().writable = writable;
    }

    pub fn patch(&self, offset: usize, data: &[u8]) {
        self.0.lock().unwrap().bytes[offset..offset + data.len()].copy_from_slice(data);
    }
}

impl BlockRead for Disk {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> CoreResult<()> {
        let mut state = self.0.lock().unwrap();
        if let Some(budget) = state.read_budget.as_mut() {
            if *budget == 0 {
                return Err(CoreError::Custom("read refused".into()));
            }
            *budget -= 1;
        }
        let size = state.bytes.len() as u64;
        let out_of_bounds = CoreError::OutOfBounds {
            offset,
            len: buf.len() as u64,
            size,
        };
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|end| *end <= size)
            .ok_or(out_of_bounds)?;
        buf.copy_from_slice(&state.bytes[offset as usize..end as usize]);
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.0.lock().unwrap().bytes.len() as u64
    }
}

impl BlockDevice for Disk {
    fn write_at(&self, offset: u64, buf: &[u8]) -> CoreResult<()> {
        let mut state = self.0.lock().unwrap();
        state.writes += 1;
        if !state.writable {
            return Err(CoreError::ReadOnly);
        }
        if let Some(budget) = state.write_budget.as_mut() {
            if *budget == 0 {
                return Err(CoreError::Custom("write refused".into()));
            }
            *budget -= 1;
        }
        state.events.push(Event::Write {
            offset,
            length: buf.len(),
        });
        let size = state.bytes.len() as u64;
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|end| *end <= size)
            .ok_or(CoreError::OutOfBounds {
                offset,
                len: buf.len() as u64,
                size,
            })?;
        state.bytes[offset as usize..end as usize].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&self) -> CoreResult<()> {
        let mut state = self.0.lock().unwrap();
        state.flushes += 1;
        state.events.push(Event::Flush);
        Ok(())
    }

    fn is_writable(&self) -> bool {
        self.0.lock().unwrap().writable
    }

    fn set_len(&self, _new_len: u64) -> CoreResult<()> {
        self.0.lock().unwrap().set_lens += 1;
        Err(CoreError::ReadOnly)
    }
}

/// Where the parts of a built image are.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub kind: FatKind,
    pub sector: usize,
    pub reserved: usize,
    pub fat_sectors: usize,
    pub fats: usize,
    pub root_sectors: usize,
    pub total_sectors: usize,
    pub clusters: usize,
}

impl Layout {
    pub fn fat_offset(&self, copy: usize) -> usize {
        (self.reserved + copy * self.fat_sectors) * self.sector
    }

    pub fn root_offset(&self) -> usize {
        (self.reserved + self.fats * self.fat_sectors) * self.sector
    }

    pub fn data_offset(&self) -> usize {
        self.root_offset() + self.root_sectors * self.sector
    }

    pub fn cluster_offset(&self, cluster: usize) -> usize {
        self.data_offset() + (cluster - 2) * self.sector
    }
}

/// A small valid image of the given variant with one sector per cluster and an
/// empty root directory.
pub fn image(kind: FatKind) -> (Vec<u8>, Layout) {
    let (total, fat_sectors, root_entries, reserved) = match kind {
        FatKind::Fat12 => (4000usize, 12usize, 224usize, 1usize),
        FatKind::Fat16 => (20_000, 80, 512, 1),
        FatKind::Fat32 => (70_000, 560, 0, 32),
    };
    let sector = 512;
    let fats = 2;
    let root_sectors = root_entries * 32 / sector;
    let clusters = total - reserved - fats * fat_sectors - root_sectors;
    let layout = Layout {
        kind,
        sector,
        reserved,
        fat_sectors,
        fats,
        root_sectors,
        total_sectors: total,
        clusters,
    };
    let mut bytes = vec![0u8; total * sector];
    let boot = &mut bytes[..sector];
    boot[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    boot[3..11].copy_from_slice(b"TESTFAT ");
    boot[11..13].copy_from_slice(&(sector as u16).to_le_bytes());
    boot[13] = 1;
    boot[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
    boot[16] = fats as u8;
    boot[17..19].copy_from_slice(&(root_entries as u16).to_le_bytes());
    boot[21] = 0xF8;
    match kind {
        FatKind::Fat32 => {
            boot[32..36].copy_from_slice(&(total as u32).to_le_bytes());
            boot[36..40].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
            boot[44..48].copy_from_slice(&2u32.to_le_bytes());
            boot[48..50].copy_from_slice(&1u16.to_le_bytes());
            boot[50..52].copy_from_slice(&6u16.to_le_bytes());
            boot[66] = 0x29;
            boot[67..71].copy_from_slice(&0x1234_5678u32.to_le_bytes());
            boot[71..82].copy_from_slice(b"NO NAME    ");
            boot[82..90].copy_from_slice(b"FAT32   ");
        }
        _ => {
            boot[19..21].copy_from_slice(&(total as u16).to_le_bytes());
            boot[22..24].copy_from_slice(&(fat_sectors as u16).to_le_bytes());
            boot[38] = 0x29;
            boot[39..43].copy_from_slice(&0x1234_5678u32.to_le_bytes());
            boot[43..54].copy_from_slice(b"NO NAME    ");
            boot[54..62].copy_from_slice(if kind == FatKind::Fat12 {
                b"FAT12   "
            } else {
                b"FAT16   "
            });
        }
    }
    boot[510..512].copy_from_slice(&[0x55, 0xAA]);
    for copy in 0..fats {
        let fat = layout.fat_offset(copy);
        match kind {
            FatKind::Fat12 => bytes[fat..fat + 3].copy_from_slice(&[0xF8, 0xFF, 0xFF]),
            FatKind::Fat16 => {
                bytes[fat..fat + 4].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0xFF]);
            }
            FatKind::Fat32 => {
                bytes[fat..fat + 12].copy_from_slice(&[
                    0xF8, 0xFF, 0xFF, 0x0F, 0xFF, 0xFF, 0xFF, 0x0F, 0xFF, 0xFF, 0xFF, 0x0F,
                ]);
            }
        }
    }
    (bytes, layout)
}

impl Layout {
    /// Sets one FAT entry in every copy of an image held in memory.
    pub fn set_entry(&self, bytes: &mut [u8], cluster: usize, value: u32) {
        for copy in 0..self.fats {
            let base = self.fat_offset(copy);
            match self.kind {
                FatKind::Fat12 => {
                    let at = base + cluster * 3 / 2;
                    let word = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
                    let word = if cluster.is_multiple_of(2) {
                        (word & 0xF000) | value as u16
                    } else {
                        (word & 0x000F) | ((value as u16) << 4)
                    };
                    bytes[at..at + 2].copy_from_slice(&word.to_le_bytes());
                }
                FatKind::Fat16 => {
                    let at = base + cluster * 2;
                    bytes[at..at + 2].copy_from_slice(&(value as u16).to_le_bytes());
                }
                FatKind::Fat32 => {
                    let at = base + cluster * 4;
                    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
                }
            }
        }
    }

    /// Writes a file of contiguous clusters into root directory slot `slot`.
    pub fn add_root_file(
        &self,
        bytes: &mut [u8],
        slot: usize,
        name: &[u8; 11],
        first_cluster: usize,
        content: &[u8],
    ) {
        let root = match self.kind {
            FatKind::Fat32 => self.cluster_offset(2),
            _ => self.root_offset(),
        };
        let entry = &mut bytes[root + slot * 32..root + slot * 32 + 32];
        entry[..11].copy_from_slice(name);
        entry[11] = 0x20;
        entry[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
        entry[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
        entry[28..32].copy_from_slice(&(content.len() as u32).to_le_bytes());
        let clusters = content.len().div_ceil(self.sector).max(1);
        let end = match self.kind {
            FatKind::Fat12 => 0xFFF,
            FatKind::Fat16 => 0xFFFF,
            FatKind::Fat32 => 0x0FFF_FFFF,
        };
        for index in 0..clusters {
            let cluster = first_cluster + index;
            let next = if index + 1 == clusters {
                end
            } else {
                cluster as u32 + 1
            };
            self.set_entry(bytes, cluster, next);
        }
        let at = self.cluster_offset(first_cluster);
        bytes[at..at + content.len()].copy_from_slice(content);
    }
}

/// A deterministic byte pattern.
pub fn pattern(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A named way of damaging an image.
pub type Damage = Box<dyn Fn(&mut Vec<u8>)>;
