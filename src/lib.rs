#![deny(unsafe_code)]

pub mod boot;
pub mod directory;
pub mod file;
pub mod mkfs;
pub mod primitives;
pub mod timestamp;
pub mod volume;
pub mod write;

pub use boot::{BootSector, BootWarning, FatKind};
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
    /// No file or directory has that path.
    NotFound,
    /// A file or directory with that name (compared without regard to case,
    /// and including generated short names) already exists.
    Exists,
    /// A directory was needed and a file was found.
    NotADirectory,
    /// A file was needed and a directory was found.
    IsADirectory,
    /// The directory still holds entries.
    NotEmpty,
    /// The volume has no free cluster left for the change.
    VolumeFull,
    /// The fixed FAT12/FAT16 root directory has no room for the entry.
    RootDirectoryFull,
    /// The directory has reached the 65536 entries FAT allows.
    DirectoryFull,
    /// The name cannot be stored in a FAT directory.
    InvalidName(&'static str),
    /// The handle's file has been removed, replaced or renamed since it was
    /// opened, so it no longer names that file.
    StaleHandle,
    /// The handle belongs to another volume.
    ForeignHandle,
    /// FAT files hold at most 4 GiB less one byte.
    FileTooLarge,
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
            Self::NotFound => write!(formatter, "no such file or directory"),
            Self::Exists => write!(formatter, "a file or directory with that name exists"),
            Self::NotADirectory => write!(formatter, "not a directory"),
            Self::IsADirectory => write!(formatter, "is a directory"),
            Self::NotEmpty => write!(formatter, "directory is not empty"),
            Self::VolumeFull => write!(formatter, "the FAT volume is full"),
            Self::RootDirectoryFull => write!(formatter, "the fixed root directory is full"),
            Self::DirectoryFull => write!(formatter, "the directory is full"),
            Self::InvalidName(reason) => write!(formatter, "invalid FAT name: {reason}"),
            Self::StaleHandle => write!(formatter, "the handle no longer names its file"),
            Self::ForeignHandle => write!(formatter, "the handle belongs to another volume"),
            Self::FileTooLarge => write!(formatter, "a FAT file holds at most 4 GiB less one byte"),
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
