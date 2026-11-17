//! `cat` and `export`: a file's bytes, through the library's `FileAPI`.

use super::list::{path_arg, path_of};
use super::target::{failure, Target};
use super::tree::{self, Resolved};
use super::Verb;
use fs_core::cli::clap::{ArgMatches, Command};
use fs_core::cli::{transfer, CliError, Outcome};
use fs_fat32::directory::DirectoryEntry;
use fs_fat32::file::{File, FileAPI};
use std::io::Write;
use std::path::Path;

pub const CAT: Verb = Verb {
    name: "cat",
    command: cat_command,
    run: cat_run,
};

pub const EXPORT: Verb = Verb {
    name: "export",
    command: export_command,
    run: export_run,
};

fn cat_command() -> Command {
    Command::new("cat")
        .about("Write a file's bytes to stdout")
        .arg(path_arg(true))
}

fn export_command() -> Command {
    Command::new("export")
        .about("Atomically extract a file to a local file; --verify checks the staged bytes before publication")
        .args(transfer::export_args_with_verify())
}

fn file_entry(volume: &super::target::Vol, path: &str) -> Result<DirectoryEntry, CliError> {
    match tree::resolve(volume, path)? {
        Resolved::Entry(entry) if !tree::is_directory(&entry) => Ok(entry),
        _ => Err(CliError::failed(format!("{path}: is a directory"))),
    }
}

fn cat_run(target: &Target, arguments: &ArgMatches) -> Result<Outcome, CliError> {
    let path = path_of(arguments)?;
    let volume = target.volume()?;
    let entry = file_entry(&volume, &path)?;
    let file = volume.open_file(&entry).map_err(failure)?;
    let mut stdout = std::io::stdout().lock();
    let mut buffer = vec![0; transfer::CHUNK];
    let mut position = 0;
    while position < file.len() {
        let count = file.read_at(position, &mut buffer).map_err(failure)?;
        if count == 0 {
            return Err(CliError::failed("short file read"));
        }
        stdout
            .write_all(&buffer[..count])
            .map_err(|problem| CliError::failed(problem.to_string()))?;
        position += count as u64;
    }
    stdout
        .flush()
        .map_err(|problem| CliError::failed(problem.to_string()))?;
    Ok(Outcome::done())
}

struct ExportSource<'volume> {
    file: File<'volume, super::target::Patched>,
    image: &'volume Path,
}

impl transfer::FileSource for ExportSource<'_> {
    fn image_path(&self) -> Option<&Path> {
        Some(self.image)
    }

    fn size(&self) -> u64 {
        self.file.len()
    }

    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, CliError> {
        self.file.read_at(offset, output).map_err(failure)
    }
}

fn export_run(target: &Target, arguments: &ArgMatches) -> Result<Outcome, CliError> {
    let request = transfer::ExportRequest::from_matches(arguments)?;
    let options = transfer::ExportOptions::from_matches(arguments)?;
    let path = request
        .path
        .to_str()
        .ok_or_else(|| CliError::usage("a FAT path must be UTF-8"))?
        .to_string();
    let volume = target.volume()?;
    let entry = file_entry(&volume, &path)?;
    let file = volume.open_file(&entry).map_err(failure)?;
    let mut source = ExportSource {
        file,
        image: &target.image,
    };
    transfer::export_with_options(&request, &mut source, options)
}
