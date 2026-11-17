//! `fs.fat`: look inside a FAT12, FAT16 or FAT32 image without mounting it.
//!
//! Every verb here only reads. The image is opened read-only, and a volume
//! whose boot sector is damaged is still inspectable: `info` reads it as raw
//! bytes through `RawVolume`, and `--boot-sector` lets the other verbs use a
//! backup copy of the boot sector instead of the damaged one.
//!
//! ADDING A VERB is one entry in [`VERBS`] and one module. A verb is a clap
//! command and a function given the [`Target`]; it opens what it needs, so a
//! verb that writes (file operations, a later change) opens its own writable
//! device and leaves the read-only verbs as they are. A tool of its own
//! (`mkfs`, the checker) is not a verb: it is another entry in `FAMILY.tools`.

mod content;
mod info;
mod list;
mod scan;
mod target;
mod tree;

use fs_core::cli::clap::{ArgMatches, Command};
use fs_core::cli::{self, CliError, Outcome, Tool};
use target::Target;

/// One word after `fs.fat IMAGE`.
pub struct Verb {
    pub name: &'static str,
    pub command: fn() -> Command,
    pub run: fn(&Target, &ArgMatches) -> Result<Outcome, CliError>,
}

/// Every verb, in the order help lists them.
static VERBS: &[Verb] = &[
    info::INFO,
    list::LS,
    list::STAT,
    content::CAT,
    content::EXPORT,
    scan::SCAN,
    scan::CHAIN,
    scan::FAT,
];

pub const TOOL: Tool = Tool {
    name: "fs.fat",
    verb: "fs",
    section: 1,
    about: "Inspect, list, read and extract files, and dump forensic records, without mounting",
    usage_exit: cli::output::EXIT_USAGE,
    command,
    run,
};

fn command() -> Command {
    let mut command = Command::new("fs.fat")
        .about("Read a FAT12, FAT16 or FAT32 image or device without mounting it; it is only opened read-only")
        .args(target::target_args())
        .args(cli::format_args().map(|argument| argument.global(true)))
        .subcommand_required(true);
    for verb in VERBS {
        command = command.subcommand((verb.command)());
    }
    command.after_help(
        "Examples:\n  \
         fs.fat disk.img info\n  \
         fs.fat disk.img ls /\n  \
         fs.fat disk.img ls -R /docs\n  \
         fs.fat disk.img stat /docs/guide.txt\n  \
         fs.fat disk.img cat /docs/guide.txt\n  \
         fs.fat disk.img export /docs/guide.txt guide.txt --verify\n  \
         fs.fat disk.img scan --deleted -R /\n  \
         fs.fat disk.img chain 7\n  \
         fs.fat damaged.img info\n  \
         fs.fat damaged.img --boot-sector 6 ls /\n\n\
         Every verb opens the image read-only and never writes to it, to the volume or to anything \
         beside it. info reads the boot sector as raw bytes and reports what is wrong with it when \
         it is damaged, and where a backup copy is; the other verbs need a boot sector that holds \
         up, which --boot-sector SECTOR takes from a backup copy. Paths run from the root of the \
         volume, and names match without regard to case, as FAT does. This tool reads only; the \
         write verbs, the checker and the formatter are separate work (README.md).",
    )
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    let target = Target::from_matches(matches)?;
    let (name, arguments) = matches
        .subcommand()
        .ok_or_else(|| CliError::usage("missing verb"))?;
    let verb = VERBS
        .iter()
        .find(|verb| verb.name == name)
        .ok_or_else(|| CliError::usage("unknown verb"))?;
    (verb.run)(&target, arguments)
}

/// Lower-case hex, for raw bytes in a report.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
