//! Reading directories and resolving paths with the library's public pieces.
//!
//! The library lists the root; a subdirectory is its cluster chain read as
//! bytes and parsed, which is what `PrimitiveAPI::chain_candidates`,
//! `Volume::read_cluster` and `directory::parse_directory` are for. The
//! walk, the path lookup and the byte-to-disk mapping the forensic scan
//! needs live here, so the library stays as it is.

use super::target::{failure, Vol};
use fs_core::cli::listing::{self, ListBounds};
use fs_core::cli::CliError;
use fs_fat32::directory::{parse_directory, DirectoryEntry};
use fs_fat32::volume::VolumeAPI;
use fs_fat32::{Error, FatKind};
use std::collections::HashSet;

pub const ATTR_DIRECTORY: u8 = 0x10;

/// A directory: the root, or the one whose first cluster an entry names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dir {
    Root,
    Cluster(u32),
}

/// A run of a directory's bytes and where it is in the volume.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub directory_offset: u64,
    pub volume_offset: u64,
    pub length: u64,
}

/// A directory read whole: every byte, including what follows an end marker.
pub struct DirectoryBytes {
    pub bytes: Vec<u8>,
    pub spans: Vec<Span>,
}

impl DirectoryBytes {
    /// Where the byte at `directory_offset` is in the volume.
    pub fn volume_offset(&self, directory_offset: u64) -> Option<u64> {
        self.spans.iter().find_map(|span| {
            (directory_offset >= span.directory_offset
                && directory_offset < span.directory_offset + span.length)
                .then(|| span.volume_offset + (directory_offset - span.directory_offset))
        })
    }
}

pub fn read_directory(volume: &Vol, dir: Dir) -> Result<DirectoryBytes, CliError> {
    read(volume, dir).map_err(failure)
}

fn read(volume: &Vol, dir: Dir) -> Result<DirectoryBytes, Error> {
    let boot = volume.boot();
    let first = match dir {
        Dir::Root if boot.kind != FatKind::Fat32 => {
            // FAT12/16: a fixed run of sectors between the FATs and the data.
            let start = (u64::from(boot.reserved_sectors)
                + u64::from(boot.fat_count) * u64::from(boot.sectors_per_fat))
                * u64::from(boot.bytes_per_sector);
            let length = u64::from(boot.root_dir_sectors) * u64::from(boot.bytes_per_sector);
            let mut bytes = vec![0; usize::try_from(length).map_err(|_| Error::Bounds)?];
            volume.read_volume_bytes(start, &mut bytes)?;
            return Ok(DirectoryBytes {
                bytes,
                spans: vec![Span {
                    directory_offset: 0,
                    volume_offset: start,
                    length,
                }],
            });
        }
        Dir::Root => boot.root_cluster,
        Dir::Cluster(first) => first,
    };
    let clusters = volume.chain_candidates(first, boot.cluster_count as usize)?;
    if clusters.is_empty() {
        return Err(Error::Invalid("directory starts outside the data region"));
    }
    let size = boot.cluster_size();
    let mut bytes = Vec::with_capacity(clusters.len() * size as usize);
    let mut spans = Vec::with_capacity(clusters.len());
    for cluster in clusters {
        let mut data = vec![0; size as usize];
        volume.read_cluster(cluster, &mut data)?;
        spans.push(Span {
            directory_offset: bytes.len() as u64,
            volume_offset: volume.cluster_offset(cluster)?,
            length: size,
        });
        bytes.extend(data);
    }
    Ok(DirectoryBytes { bytes, spans })
}

fn is_dot(name: &str) -> bool {
    name == "." || name == ".."
}

/// The live entries of a directory, as the library's normal parser reads it:
/// up to the end marker, deleted ones left out, `.` and `..` left out.
pub fn live_entries(volume: &Vol, dir: Dir) -> Result<Vec<DirectoryEntry>, CliError> {
    let bytes = read_directory(volume, dir)?;
    Ok(parse_directory(&bytes.bytes, volume.boot().kind)
        .map_err(failure)?
        .into_iter()
        .filter(|entry| !entry.deleted && !is_dot(&entry.name))
        .collect())
}

pub fn is_directory(entry: &DirectoryEntry) -> bool {
    entry.attributes & ATTR_DIRECTORY != 0
}

/// What a path names.
pub enum Resolved {
    Root,
    Entry(DirectoryEntry),
}

impl Resolved {
    pub fn dir(&self) -> Option<Dir> {
        match self {
            Resolved::Root => Some(Dir::Root),
            Resolved::Entry(entry) if is_directory(entry) => {
                Some(Dir::Cluster(entry.first_cluster))
            }
            Resolved::Entry(_) => None,
        }
    }
}

/// Find a path from the root. FAT names do not distinguish case, so an
/// exact-case match wins and any other case matches when there is none.
pub fn resolve(volume: &Vol, path: &str) -> Result<Resolved, CliError> {
    let mut current = Resolved::Root;
    let mut walked = String::new();
    for part in path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
    {
        if part == ".." {
            return Err(CliError::usage(
                "paths run from the root of the volume; `..` is not supported",
            ));
        }
        let dir = current
            .dir()
            .ok_or_else(|| CliError::failed(format!("{} is not a directory", display(&walked))))?;
        let entries = live_entries(volume, dir)?;
        let found = entries
            .iter()
            .find(|entry| entry.name == part)
            .or_else(|| {
                let wanted = part.to_lowercase();
                entries
                    .iter()
                    .find(|entry| entry.name.to_lowercase() == wanted)
            })
            .ok_or_else(|| CliError::not_found(format!("{path}: no such file or directory")))?;
        walked.push('/');
        walked.push_str(&found.name);
        current = Resolved::Entry(found.clone());
    }
    Ok(current)
}

fn display(path: &str) -> &str {
    if path.is_empty() {
        "/"
    } else {
        path
    }
}

/// An entry found by [`walk`], and whether its directory was already visited
/// (a loop in a damaged volume), in which case the walk does not enter it.
pub struct Child {
    pub entry: DirectoryEntry,
    pub revisited: bool,
}

/// One directory the walk read.
pub struct Visit<'a> {
    pub path: &'a str,
    /// A directory's own entries are depth 1.
    pub depth: u64,
    pub bytes: &'a DirectoryBytes,
    pub children: &'a [Child],
}

/// Depth first from `start`, names sorted as bytes. With `bounds` None only
/// `start` is read; with bounds, every live directory below it, never
/// entering one twice and stopping with the shared error at either bound.
pub fn walk(
    volume: &Vol,
    start: Dir,
    start_path: &str,
    bounds: Option<ListBounds>,
    visit: &mut dyn FnMut(&Visit) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let mut seen: HashSet<u32> = HashSet::new();
    if let Dir::Cluster(first) = start {
        seen.insert(first);
    }
    let mut stack = vec![(start_path.to_string(), start, 1u64)];
    while let Some((path, dir, depth)) = stack.pop() {
        let bytes = read_directory(volume, dir)?;
        let mut entries: Vec<DirectoryEntry> = parse_directory(&bytes.bytes, volume.boot().kind)
            .map_err(failure)?
            .into_iter()
            .filter(|entry| !entry.deleted && !is_dot(&entry.name))
            .collect();
        entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        let mut children = Vec::with_capacity(entries.len());
        for entry in entries {
            let revisited =
                bounds.is_some() && is_directory(&entry) && !seen.insert(entry.first_cluster);
            children.push(Child { entry, revisited });
        }
        visit(&Visit {
            path: &path,
            depth,
            bytes: &bytes,
            children: &children,
        })?;
        if let Some(bounds) = bounds {
            let below: Vec<&Child> = children
                .iter()
                .filter(|child| is_directory(&child.entry) && !child.revisited)
                .collect();
            if !below.is_empty() && depth >= bounds.max_depth {
                return Err(listing::bound_reached(
                    "--max-depth",
                    bounds.max_depth,
                    &path,
                ));
            }
            for child in below.into_iter().rev() {
                let name = if path == "/" {
                    format!("/{}", child.entry.name)
                } else {
                    format!("{path}/{}", child.entry.name)
                };
                stack.push((name, Dir::Cluster(child.entry.first_cluster), depth + 1));
            }
        }
    }
    Ok(())
}
