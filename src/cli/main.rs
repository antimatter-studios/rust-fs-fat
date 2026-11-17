//! `rust-fs-fat`: the command-line tools for FAT12, FAT16 and FAT32, one
//! multi-call binary.
//!
//! Installed as `rust-fs-fat` and linked as each dotted name. The dispatch and
//! the output contract every tool shares are `fs_core::cli` (rust-fs-core's
//! `cli` feature); `fs` is the tool itself, and a tool added later (`mkfs`,
//! the checker) is one more entry in `FAMILY.tools`, in a module of its own.

mod fs;

use fs_core::cli;
use std::process::ExitCode;

static FAMILY: cli::Family = cli::Family {
    repo: "rust-fs-fat",
    crate_name: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    about: "Read-only FAT12, FAT16 and FAT32 tools: work on an image or device directly, without mounting it",
    install_hints: &["cargo install rust-fs-fat32 --features cli"],
    tools: &[fs::TOOL],
};

fn main() -> ExitCode {
    cli::main(&FAMILY)
}
