//! Opening an image read-only, whatever state its boot sector is in.
//!
//! Every verb is given a [`Target`] and opens what it needs from it. Nothing
//! here opens a file for writing: the device is a `FileDevice` opened
//! read-only and wrapped again in `ReadOnlyDevice`, so a verb that tried to
//! write could not. A verb that writes (a later change adds them) opens its
//! own writable device; it does not change this one.

use fs_core::cli::clap::{value_parser, Arg, ArgMatches};
use fs_core::cli::CliError;
use fs_core::{BlockRead, FileDevice, OwnedSlice, ReadOnlyDevice};
use fs_fat32::volume::{RawVolume, Volume, VolumeAPI};
use fs_fat32::Error;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

/// The bytes of one sector as `Volume::open` reads the boot sector.
const SECTOR: usize = 512;

/// The image or device every verb works on, and where in it the volume is.
pub struct Target {
    pub image: PathBuf,
    /// Where the volume starts in the image, in bytes.
    pub offset: u64,
    /// A boot sector to use in place of sector 0, as a sector number in
    /// 512-byte units from the start of the volume.
    pub boot_sector: Option<u64>,
}

/// A volume opened through the library's checked layer, on a device that
/// may carry another boot sector over its own sector 0.
pub type Vol = Volume<Patched>;

/// A device whose first 512 bytes may be replaced by another boot sector
/// (the FAT32 backup, typically). Every other byte is the image's own.
pub struct Patched {
    inner: OwnedSlice,
    boot: Option<[u8; SECTOR]>,
}

impl BlockRead for Patched {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.inner.read_at(offset, buf)?;
        if let Some(boot) = &self.boot {
            let end = offset.saturating_add(buf.len() as u64);
            for position in offset..end.min(SECTOR as u64) {
                buf[(position - offset) as usize] = boot[position as usize];
            }
        }
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
}

/// The arguments every verb shares, global so they may follow the verb.
pub fn target_args() -> [Arg; 3] {
    [
        Arg::new("target")
            .value_name("IMAGE")
            .help("The image or device; it is only ever opened read-only")
            .required(true)
            .value_parser(value_parser!(OsString)),
        Arg::new("offset")
            .long("offset")
            .value_name("BYTES")
            .help("512-byte-aligned offset of the volume within IMAGE")
            .default_value("0")
            .global(true)
            .value_parser(value_parser!(u64)),
        Arg::new("boot-sector")
            .long("boot-sector")
            .value_name("SECTOR")
            .help(
                "Read the boot sector from this 512-byte sector of the volume instead of sector 0 \
                 (FAT32 keeps a backup at sector 6 when its sectors are 512 bytes; `info` says where)",
            )
            .global(true)
            .value_parser(value_parser!(u64)),
    ]
}

/// What a library error is, as the command line reports it.
pub fn failure(error: Error) -> CliError {
    CliError::failed(error.to_string())
}

impl Target {
    pub fn from_matches(matches: &ArgMatches) -> Result<Target, CliError> {
        let image = PathBuf::from(
            matches
                .get_one::<OsString>("target")
                .ok_or_else(|| CliError::usage("missing IMAGE"))?,
        );
        let offset = matches.get_one::<u64>("offset").copied().unwrap_or(0);
        if !offset.is_multiple_of(SECTOR as u64) {
            return Err(CliError::usage("--offset must be aligned to 512 bytes"));
        }
        Ok(Target {
            image,
            offset,
            boot_sector: matches.get_one::<u64>("boot-sector").copied(),
        })
    }

    fn slice(&self) -> Result<OwnedSlice, CliError> {
        let device = FileDevice::open(&self.image).map_err(|error| {
            CliError::failed(format!("cannot open {}: {error}", self.image.display()))
        })?;
        let size = device.size_bytes();
        if self.offset >= size {
            return Err(CliError::failed(
                "--offset is outside the image: there is no volume to read",
            ));
        }
        let device: Arc<dyn BlockRead> = Arc::new(ReadOnlyDevice::new(device));
        Ok(OwnedSlice::new(device, self.offset, size - self.offset))
    }

    /// The volume as bytes, which needs no boot sector at all: this is what
    /// reads a damaged one.
    pub fn raw(&self) -> Result<RawVolume<OwnedSlice>, CliError> {
        Ok(RawVolume::open(self.slice()?))
    }

    /// The volume through the checked layer. It is refused, with the reason
    /// and how to look further, when its boot sector does not hold up.
    pub fn volume(&self) -> Result<Vol, CliError> {
        let boot = match self.boot_sector {
            None => None,
            Some(sector) => {
                let mut bytes = [0u8; SECTOR];
                let at = sector
                    .checked_mul(SECTOR as u64)
                    .ok_or_else(|| CliError::usage("--boot-sector is too large"))?;
                self.raw()?
                    .read_volume_bytes(at, &mut bytes)
                    .map_err(|error| {
                        CliError::failed(format!(
                            "cannot read the boot sector at sector {sector}: {error}"
                        ))
                    })?;
                Some(bytes)
            }
        };
        let device = Patched {
            inner: self.slice()?,
            boot,
        };
        Volume::open(device).map_err(|error| match error {
            Error::Device(_) => failure(error),
            _ => CliError::refused_by_state(format!(
                "{error}; `fs.fat IMAGE info` shows the boot sector and any backup copy, and \
                 --boot-sector SECTOR reads another copy instead"
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Bytes(Vec<u8>);

    impl BlockRead for Bytes {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let start = offset as usize;
            buf.copy_from_slice(&self.0[start..start + buf.len()]);
            Ok(())
        }

        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
    }

    fn patched(boot: Option<[u8; SECTOR]>) -> Patched {
        let device: Arc<dyn BlockRead> = Arc::new(Bytes(vec![7; 1024]));
        Patched {
            inner: OwnedSlice::new(device, 0, 1024),
            boot,
        }
    }

    #[test]
    fn a_patched_boot_sector_replaces_exactly_the_first_512_bytes() {
        let device = patched(Some([1; SECTOR]));
        let mut all = vec![0; 1024];
        device.read_at(0, &mut all).unwrap();
        assert!(all[..512].iter().all(|b| *b == 1));
        assert!(all[512..].iter().all(|b| *b == 7));
        // A read that starts inside the sector and runs past it.
        let mut part = [0; 8];
        device.read_at(508, &mut part).unwrap();
        assert_eq!(part, [1, 1, 1, 1, 7, 7, 7, 7]);
        // A read wholly beyond it is the image's own.
        device.read_at(600, &mut part).unwrap();
        assert_eq!(part, [7; 8]);
    }

    #[test]
    fn without_a_patch_every_byte_is_the_images() {
        let mut all = vec![0; 1024];
        patched(None).read_at(0, &mut all).unwrap();
        assert!(all.iter().all(|b| *b == 7));
    }
}
