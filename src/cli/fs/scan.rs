//! `scan`, `chain` and `fat`: the forensic records.
//!
//! `scan` is `directory::scan_directory` over a directory's whole bytes: it
//! keeps deleted entries and records beyond an end marker, with their raw
//! bytes and where they are on disk. `chain` and `fat` are `PrimitiveAPI`:
//! the best-effort cluster chain from a cluster and why it stopped, and the
//! FAT entries as stored. None of this interprets more than the library does,
//! and none of it claims a deleted file's data is intact.

use super::list::{display_path, entry_fields, path_arg, path_of};
use super::target::{failure, Target};
use super::tree;
use super::{hex, Verb};
use fs_core::cli::clap::{value_parser, Arg, ArgAction, ArgMatches, Command};
use fs_core::cli::listing::{self, ListBounds};
use fs_core::cli::{CliError, Json, Outcome};
use fs_fat32::directory::scan_directory;
use fs_fat32::primitives::{FatValue, PrimitiveAPI};

pub const SCAN: Verb = Verb {
    name: "scan",
    command: scan_command,
    run: scan_run,
};

pub const CHAIN: Verb = Verb {
    name: "chain",
    command: chain_command,
    run: chain_run,
};

pub const FAT: Verb = Verb {
    name: "fat",
    command: fat_command,
    run: fat_run,
};

fn scan_command() -> Command {
    Command::new("scan")
        .about("Dump a directory's raw records, including deleted entries and records past its end marker")
        .arg(path_arg(false))
        .arg(
            Arg::new("deleted")
                .long("deleted")
                .action(ArgAction::SetTrue)
                .help("Only the records whose first byte is 0xE5, the deleted ones"),
        )
        .args(listing::recursive_args())
        .after_help(
            "Each record has the directory it is in, its offset in that directory and in the volume, \
             the entry as the library reads it, and its 32 raw bytes as hex. A deleted short name \
             shows `_` for the lost first character; a deleted long-name sequence is not guessed. \
             The first cluster of a deleted file is whatever the record still says: `chain` shows \
             what the FAT holds from there, and it may belong to another file by now. With \
             --recursive the live subdirectories are scanned too.",
        )
}

fn scan_run(target: &Target, arguments: &ArgMatches) -> Result<Outcome, CliError> {
    let path = path_of(arguments)?;
    let volume = target.volume()?;
    let resolved = tree::resolve(&volume, &path)?;
    let dir = resolved
        .dir()
        .ok_or_else(|| CliError::failed(format!("{path}: is not a directory")))?;
    let only_deleted = arguments.get_flag("deleted");
    let recursive = arguments.get_flag("recursive");
    let bounds = recursive.then(|| ListBounds::from_matches(arguments));
    let limit = bounds.map_or(listing::LIMIT, |bounds| bounds.limit);
    let kind = volume.boot().kind;
    let mut records = Vec::new();
    tree::walk(&volume, dir, &display_path(&path), bounds, &mut |visit| {
        for record in scan_directory(&visit.bytes.bytes, kind).map_err(failure)? {
            if only_deleted && record.raw[0] != 0xE5 {
                continue;
            }
            if records.len() as u64 >= limit {
                return Err(listing::bound_reached("--limit", limit, visit.path));
            }
            let mut fields = vec![
                ("directory", Json::from(visit.path)),
                ("directory_offset", Json::from(record.offset as u64)),
                (
                    "volume_offset",
                    Json::from(visit.bytes.volume_offset(record.offset as u64)),
                ),
                ("deleted", Json::from(record.entry.deleted)),
            ];
            fields.extend(entry_fields(&record.entry));
            fields.push(("raw", Json::from(hex(&record.raw))));
            records.push(Json::object(fields));
        }
        Ok(())
    })?;
    Ok(Outcome::report(Json::Arr(records)))
}

fn cluster_arg() -> Arg {
    Arg::new("cluster")
        .value_name("CLUSTER")
        .help("A cluster number, 2 or more")
        .required(true)
        .value_parser(value_parser!(u32))
}

fn bound_arg(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .value_name("N")
        .help(help)
        .value_parser(value_parser!(u64))
}

fn chain_command() -> Command {
    Command::new("chain")
        .about("Follow the FAT from a cluster: the best-effort chain and why it stopped")
        .arg(cluster_arg())
        .arg(bound_arg(
            "limit",
            "Stop after this many clusters (default: 100000)",
        ))
        .after_help(
            "The chain is what the active FAT holds, stopped safely: `stopped` is end_of_chain, free, \
             bad, reserved, out_of_range, loop or limit. A cleared chain (a deleted file's) reads as \
             free and stops at once; nothing here guesses the rest.",
        )
}

fn fat_value_json(value: FatValue) -> (&'static str, Json) {
    match value {
        FatValue::Next(next) => ("next", Json::from(next)),
        FatValue::EndOfChain => ("end_of_chain", Json::Null),
        FatValue::Free => ("free", Json::Null),
        FatValue::Reserved(value) => ("reserved", Json::from(value)),
        FatValue::Bad => ("bad", Json::Null),
        FatValue::OutOfRange(value) => ("out_of_range", Json::from(value)),
    }
}

fn chain_run(target: &Target, arguments: &ArgMatches) -> Result<Outcome, CliError> {
    let first = *arguments.get_one::<u32>("cluster").unwrap();
    let limit = arguments
        .get_one::<u64>("limit")
        .copied()
        .unwrap_or(listing::LIMIT);
    let volume = target.volume()?;
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let clusters = volume.chain_candidates(first, limit).map_err(failure)?;
    let stopped = match clusters.last() {
        None if limit == 0 => "limit",
        None => "start_out_of_range",
        Some(last) => match volume.fat_value(*last).map_err(failure)? {
            FatValue::Next(next) if clusters.contains(&next) => "loop",
            FatValue::Next(_) => "limit",
            other => fat_value_json(other).0,
        },
    };
    Ok(Outcome::report(Json::object([
        ("start", Json::from(first)),
        ("count", Json::from(clusters.len() as u64)),
        ("clusters", Json::from(clusters)),
        ("stopped", Json::from(stopped)),
    ])))
}

fn fat_command() -> Command {
    Command::new("fat")
        .about("Show FAT entries as stored: the raw value and what it means")
        .arg(cluster_arg())
        .arg(bound_arg(
            "count",
            "How many entries, from CLUSTER (default: 1, at most 100000)",
        ))
        .after_help("Reads the active FAT only. `value` is next, end_of_chain, free, reserved, bad or out_of_range.")
}

fn fat_run(target: &Target, arguments: &ArgMatches) -> Result<Outcome, CliError> {
    let first = *arguments.get_one::<u32>("cluster").unwrap();
    let count = arguments.get_one::<u64>("count").copied().unwrap_or(1);
    if count == 0 || count > listing::LIMIT {
        return Err(CliError::usage("--count must be from 1 to 100000"));
    }
    let volume = target.volume()?;
    let mut entries = Vec::new();
    for offset in 0..count {
        let cluster = u64::from(first) + offset;
        let cluster = u32::try_from(cluster)
            .map_err(|_| CliError::usage("the cluster range passes the largest cluster number"))?;
        let raw = volume.fat_entry_raw(cluster).map_err(|error| {
            CliError::failed(format!(
                "FAT entry of cluster {cluster}: {}",
                failure(error).message
            ))
        })?;
        let (value, detail) = fat_value_json(volume.fat_value(cluster).map_err(failure)?);
        entries.push(Json::object([
            ("cluster", Json::from(cluster)),
            ("raw", Json::from(format!("0x{raw:08x}"))),
            ("value", Json::from(value)),
            ("detail", detail),
        ]));
    }
    Ok(Outcome::report(Json::Arr(entries)))
}
