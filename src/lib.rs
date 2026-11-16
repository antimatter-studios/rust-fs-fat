#![deny(unsafe_code)]

pub mod boot;
pub mod directory;
pub mod file;
pub mod primitives;
pub mod volume;

pub use boot::{BootSector, FatKind};
pub use volume::{RawVolume, Volume};

#[derive(Debug)]
pub enum Error {
    Device(fs_core::Error),
    Invalid(&'static str),
    Bounds,
    Unsupported(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Device(error) => write!(formatter, "{error}"),
            Self::Invalid(reason) => write!(formatter, "invalid FAT volume: {reason}"),
            Self::Bounds => write!(formatter, "FAT volume address out of bounds"),
            Self::Unsupported(reason) => write!(formatter, "unsupported FAT volume: {reason}"),
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
