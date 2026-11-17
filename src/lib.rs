#![deny(unsafe_code)]

pub mod boot;
pub mod directory;
pub mod file;
pub mod primitives;
pub mod volume;
pub mod write;

pub use boot::{BootSector, FatKind};
pub use volume::{RawVolume, Volume};
pub use write::{RawWriteVolume, WriteVolume};

#[derive(Debug)]
pub enum Error {
    Device(fs_core::Error),
    Invalid(&'static str),
    Bounds,
    Unsupported(&'static str),
    /// The volume is marked dirty or reports errors, so a writer refuses it.
    Dirty(&'static str),
    /// An earlier write failed partway; the mount takes no more writes and
    /// leaves the volume marked dirty.
    Failed,
    /// The mount has been closed.
    Closed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Device(error) => write!(formatter, "{error}"),
            Self::Invalid(reason) => write!(formatter, "invalid FAT volume: {reason}"),
            Self::Bounds => write!(formatter, "FAT volume address out of bounds"),
            Self::Unsupported(reason) => write!(formatter, "unsupported FAT volume: {reason}"),
            Self::Dirty(reason) => write!(formatter, "FAT volume refused for writing: {reason}"),
            Self::Failed => write!(
                formatter,
                "an earlier write failed partway; the volume is left marked dirty"
            ),
            Self::Closed => write!(formatter, "FAT volume is closed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<fs_core::Error> for Error {
    fn from(error: fs_core::Error) -> Self {
        Self::Device(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
