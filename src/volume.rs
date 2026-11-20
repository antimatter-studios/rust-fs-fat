use crate::boot::{BootSector, BootWarning};
use crate::{Error, Result};
use fs_core::{BlockRead, FileDevice, OwnedSlice, ReadOnlyDevice};
use std::path::Path;
use std::sync::Arc;

/// The volume layer this crate had before `fs_core::layers`.
///
/// Superseded by `fs_core::layers::VolumeAPI`, which [`Volume`] now also
/// implements (see [`crate::layers`]). This trait stays for one release as a
/// thin wrapper over the same reads.
#[deprecated(
    since = "0.2.0",
    note = "use fs_core::layers::VolumeAPI, implemented by fs_fat32::Volume; removed in the next release"
)]
pub trait VolumeAPI {
    fn size_bytes(&self) -> u64;
    fn read_volume_bytes(&self, offset: u64, output: &mut [u8]) -> Result<()>;
}

pub struct RawVolume<Device: BlockRead> {
    device: Device,
}

impl<Device: BlockRead> RawVolume<Device> {
    pub fn open(device: Device) -> Self {
        Self { device }
    }
}

#[allow(deprecated)]
impl<Device: BlockRead> VolumeAPI for RawVolume<Device> {
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

pub struct Volume<Device: BlockRead> {
    device: Device,
    boot: BootSector,
    /// Where the boot sector that was read lies: 0 for the primary, or the
    /// byte offset of the FAT32 backup a salvage open fell back to.
    boot_at: u64,
    /// The bytes this volume can read: the volume length its boot sector
    /// declares, and no more than the device holds.
    size: u64,
}

impl<Device: BlockRead> Volume<Device> {
    pub fn open(device: Device) -> Result<Self> {
        let mut sector = [0u8; 512];
        device.read_at(0, &mut sector)?;
        let boot = BootSector::parse(&sector)?;
        let volume_size = u64::from(boot.total_sectors) * u64::from(boot.bytes_per_sector);
        if volume_size > device.size_bytes() {
            return Err(Error::Invalid("BPB volume length exceeds device"));
        }
        Ok(Self {
            size: volume_size,
            device,
            boot,
            boot_at: 0,
        })
    }

    /// Opens a volume for salvage: a boot sector that fails to read or does
    /// not decode is replaced by the FAT32 backup, and a volume longer than
    /// its device (a truncated image) opens as far as the device goes.
    pub(crate) fn open_salvage(device: Device) -> Result<Self> {
        if device.size_bytes() < 512 {
            return Err(Error::Invalid("device is shorter than a boot sector"));
        }
        let mut sector = [0u8; 512];
        let primary = device
            .read_at(0, &mut sector)
            .map_err(Error::from)
            .and_then(|()| BootSector::parse(&sector));
        let (boot, boot_at) = match primary {
            Ok(boot) => (boot, 0),
            Err(error) => {
                let backup = backup_boot(&device, &sector).ok_or(error)?;
                (backup.0, backup.1)
            }
        };
        let declared = u64::from(boot.total_sectors) * u64::from(boot.bytes_per_sector);
        Ok(Self {
            size: declared.min(device.size_bytes()),
            device,
            boot,
            boot_at,
        })
    }

    /// The byte offset of the boot sector this volume was read from: 0, or
    /// the FAT32 backup a salvage open fell back to.
    pub fn boot_sector_offset(&self) -> u64 {
        self.boot_at
    }

    pub fn boot(&self) -> &BootSector {
        &self.boot
    }

    /// What is wrong with the boot sector that the reader tolerated to open
    /// this volume. Empty for a volume with nothing to report.
    pub fn warnings(&self) -> Vec<BootWarning> {
        self.boot.warnings()
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    pub fn cluster_offset(&self, cluster: u32) -> Result<u64> {
        if cluster < 2 || cluster >= self.boot.cluster_count + 2 {
            return Err(Error::Bounds);
        }
        let sector = u64::from(self.boot.first_data_sector)
            .checked_add(u64::from(cluster - 2) * u64::from(self.boot.sectors_per_cluster))
            .ok_or(Error::Bounds)?;
        Ok(sector * u64::from(self.boot.bytes_per_sector))
    }

    pub fn read_cluster(&self, cluster: u32, output: &mut [u8]) -> Result<()> {
        if output.len() as u64 != self.boot.cluster_size() {
            return Err(Error::Invalid("cluster buffer has wrong length"));
        }
        self.read_bytes(self.cluster_offset(cluster)?, output)
    }

    /// The bytes this volume can read: its declared length, or the device's
    /// when a salvage open found the device shorter.
    pub fn len(&self) -> u64 {
        self.size
    }

    /// Whether the volume reads no bytes at all.
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Reads volume bytes; the same read as `read_volume_bytes` on either
    /// `VolumeAPI`, without needing a trait in scope.
    pub fn read_bytes(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(output.len() as u64)
            .ok_or(Error::Bounds)?;
        if end > self.size {
            return Err(Error::Bounds);
        }
        self.device.read_at(offset, output)?;
        Ok(())
    }
}

#[allow(deprecated)]
impl<Device: BlockRead> VolumeAPI for Volume<Device> {
    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn read_volume_bytes(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        self.read_bytes(offset, output)
    }
}

/// The FAT32 backup boot sector of a volume whose primary does not decode,
/// and where it was found. The primary's own field says where the backup is
/// when that byte survives; sector 6 is where every formatter puts it.
pub(crate) fn backup_boot(
    device: &dyn BlockRead,
    primary: &[u8; 512],
) -> Option<(BootSector, u64)> {
    let named = u64::from(u16::from_le_bytes([primary[50], primary[51]]));
    let sector_sizes = {
        let named = u64::from(u16::from_le_bytes([primary[11], primary[12]]));
        [named, 512, 1024, 2048, 4096]
    };
    for sector_size in sector_sizes {
        if !matches!(sector_size, 512 | 1024 | 2048 | 4096) {
            continue;
        }
        for sector in [named, 6] {
            let offset = sector * sector_size;
            if sector == 0 || offset + 512 > device.size_bytes() {
                continue;
            }
            let mut bytes = [0u8; 512];
            if device.read_at(offset, &mut bytes).is_err() {
                continue;
            }
            if let Ok(boot) = BootSector::parse(&bytes) {
                if boot.kind == crate::FatKind::Fat32
                    && u64::from(boot.bytes_per_sector) == sector_size
                {
                    return Some((boot, offset));
                }
            }
        }
    }
    None
}

pub fn open_device<Device: BlockRead + 'static>(
    device: Device,
    offset: u64,
) -> Result<Volume<OwnedSlice>> {
    if !offset.is_multiple_of(512) || offset >= device.size_bytes() {
        return Err(Error::Invalid(
            "volume offset must be aligned and within device",
        ));
    }
    let length = device.size_bytes() - offset;
    let device: Arc<dyn BlockRead> = Arc::new(ReadOnlyDevice::new(device));
    Volume::open(OwnedSlice::new(device, offset, length))
}

pub fn open_raw_device<Device: BlockRead + 'static>(
    device: Device,
    offset: u64,
) -> Result<RawVolume<OwnedSlice>> {
    if !offset.is_multiple_of(512) || offset >= device.size_bytes() {
        return Err(Error::Invalid(
            "volume offset must be aligned and within device",
        ));
    }
    let length = device.size_bytes() - offset;
    let device: Arc<dyn BlockRead> = Arc::new(ReadOnlyDevice::new(device));
    Ok(RawVolume::open(OwnedSlice::new(device, offset, length)))
}

pub fn open_path(path: &Path, offset: u64) -> Result<Volume<OwnedSlice>> {
    open_device(FileDevice::open(path)?, offset)
}
