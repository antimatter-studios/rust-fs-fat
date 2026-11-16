use crate::boot::BootSector;
use crate::{Error, Result};
use fs_core::{BlockRead, FileDevice, OwnedSlice, ReadOnlyDevice};
use std::path::Path;
use std::sync::Arc;

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
        Ok(Self { device, boot })
    }

    pub fn boot(&self) -> &BootSector {
        &self.boot
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
        self.read_volume_bytes(self.cluster_offset(cluster)?, output)
    }
}

impl<Device: BlockRead> VolumeAPI for Volume<Device> {
    fn size_bytes(&self) -> u64 {
        u64::from(self.boot.total_sectors) * u64::from(self.boot.bytes_per_sector)
    }

    fn read_volume_bytes(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(output.len() as u64)
            .ok_or(Error::Bounds)?;
        let volume_end = u64::from(self.boot.total_sectors) * u64::from(self.boot.bytes_per_sector);
        if end > volume_end {
            return Err(Error::Bounds);
        }
        self.device.read_at(offset, output)?;
        Ok(())
    }
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
