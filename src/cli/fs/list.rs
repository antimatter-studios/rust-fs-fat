//! `ls` and `stat`: what a path holds, as the live directory tree says.

use super::target::Target;
use super::tree::{self, is_directory, Resolved, ATTR_DIRECTORY};
use super::Verb;
use fs_core::cli::clap::{value_parser, Arg, ArgMatches, Command};
use fs_core::cli::listing::{self, ListBounds};
use fs_core::cli::{CliError, Json, Outcome};
use fs_fat32::directory::DirectoryEntry;
use std::ffi::OsString;

pub const LS: Verb = Verb {
    name: "ls",
    command: ls_command,
    run: ls_run,
};

pub const STAT: Verb = Verb {
    name: "stat",
    command: stat_command,
    run: stat_run,
};

pub fn path_arg(required: bool) -> Arg {
    let argument = Arg::new("path")
        .value_name("PATH")
        .help("A path from the root of the volume")
        .value_parser(value_parser!(OsString));
    if required {
        argument.required(true)
    } else {
        argument.default_value("/")
    }
}

pub fn path_of(arguments: &ArgMatches) -> Result<String, CliError> {
    arguments
        .get_one::<OsString>("path")
        .and_then(|path| path.to_str())
        .map(str::to_string)
        .ok_or_else(|| CliError::usage("a FAT path must be UTF-8"))
}

fn flags(attributes: u8) -> Json {
    let names = [
        (0x01, "read_only"),
        (0x02, "hidden"),
        (0x04, "system"),
        (ATTR_DIRECTORY, "directory"),
        (0x20, "archive"),
    ];
    Json::Arr(
        names
            .iter()
            .filter(|(bit, _)| attributes & bit != 0)
            .map(|(_, name)| Json::from(*name))
            .collect(),
    )
}

pub fn entry_fields(entry: &DirectoryEntry) -> Vec<(&'static str, Json)> {
    vec![
        ("name", Json::from(entry.name.as_str())),
        (
            "type",
            Json::from(if is_directory(entry) {
                "directory"
            } else {
                "file"
            }),
        ),
        ("size", Json::from(u64::from(entry.size))),
        ("attributes", Json::from(u64::from(entry.attributes))),
        ("flags", flags(entry.attributes)),
        ("first_cluster", Json::from(entry.first_cluster)),
    ]
}

fn ls_command() -> Command {
    Command::new("ls")
        .about("List a directory")
        .arg(path_arg(false))
        .args(listing::recursive_args())
        .after_help(
            "Lists the live entries: deleted entries, `.` and `..` and the volume label are left \
             out (`scan` shows what was deleted). With --recursive each entry adds its path from \
             the root of the volume and its depth, and a directory that was already entered \
             through another entry is marked `revisited` and not entered again.",
        )
}

fn ls_run(target: &Target, arguments: &ArgMatches) -> Result<Outcome, CliError> {
    let path = path_of(arguments)?;
    let volume = target.volume()?;
    let resolved = tree::resolve(&volume, &path)?;
    let Some(dir) = resolved.dir() else {
        // A file lists as itself.
        let Resolved::Entry(entry) = resolved else {
            unreachable!()
        };
        return Ok(Outcome::report(Json::Arr(vec![Json::object(
            entry_fields(&entry),
        )])));
    };
    let start = display_path(&path);
    if !arguments.get_flag("recursive") {
        let entries = tree::live_entries(&volume, dir)?;
        return Ok(Outcome::report(Json::Arr(
            entries
                .iter()
                .map(|entry| Json::object(entry_fields(entry)))
                .collect(),
        )));
    }
    let bounds = ListBounds::from_matches(arguments);
    let mut items = Vec::new();
    tree::walk(&volume, dir, &start, Some(bounds), &mut |visit| {
        for child in visit.children {
            if items.len() as u64 >= bounds.limit {
                return Err(listing::bound_reached("--limit", bounds.limit, visit.path));
            }
            let mut fields = entry_fields(&child.entry);
            let full = if visit.path == "/" {
                format!("/{}", child.entry.name)
            } else {
                format!("{}/{}", visit.path, child.entry.name)
            };
            fields.push(("path", Json::from(full)));
            fields.push(("depth", Json::from(visit.depth)));
            if child.revisited {
                fields.push(("revisited", Json::from(true)));
            }
            items.push(Json::object(fields));
        }
        Ok(())
    })?;
    Ok(Outcome::report(Json::Arr(items)))
}

/// The path a walk reports from: absolute, with no trailing slash.
pub fn display_path(path: &str) -> String {
    let parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

fn stat_command() -> Command {
    Command::new("stat")
        .about("Inspect a path: its name, type, size, attributes and first cluster")
        .arg(path_arg(true))
}

fn stat_run(target: &Target, arguments: &ArgMatches) -> Result<Outcome, CliError> {
    let path = path_of(arguments)?;
    let volume = target.volume()?;
    let report = match tree::resolve(&volume, &path)? {
        Resolved::Root => Json::object([
            ("name", Json::from("/")),
            ("type", Json::from("directory")),
            ("size", Json::from(0u64)),
            ("first_cluster", Json::from(volume.boot().root_cluster)),
        ]),
        Resolved::Entry(entry) => Json::object(entry_fields(&entry)),
    };
    Ok(Outcome::report(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walked_paths_are_absolute_without_dots_or_trailing_slashes() {
        assert_eq!(display_path(""), "/");
        assert_eq!(display_path("/"), "/");
        assert_eq!(display_path("docs//nested/"), "/docs/nested");
        assert_eq!(display_path("/./docs/."), "/docs");
    }

    #[test]
    fn attribute_flags_are_named() {
        assert_eq!(
            flags(0x21),
            Json::Arr(vec![Json::from("read_only"), Json::from("archive")])
        );
        assert_eq!(
            flags(ATTR_DIRECTORY),
            Json::Arr(vec![Json::from("directory")])
        );
    }
}
