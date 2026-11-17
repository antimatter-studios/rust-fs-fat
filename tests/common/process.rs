//! Running `rust-fs-fat` as a process and reading what it prints; shared by
//! the tests that drive the real binary.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_rust-fs-fat");

pub fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().unwrap()
}

pub fn fs(image: &Path, args: &[&str]) -> Output {
    let mut all = vec!["fs", image.to_str().unwrap()];
    all.extend_from_slice(args);
    run(&all)
}

pub fn ok(output: Output) -> String {
    assert!(
        output.status.success(),
        "exit {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// `name=... type=...` text lines of a listing as maps.
pub fn lines(text: &str) -> Vec<BTreeMap<String, String>> {
    text.lines()
        .map(|line| {
            // Values may hold spaces; every key is a bare word followed by `=`.
            let mut map = BTreeMap::new();
            let mut value = String::new();
            let mut pending: Option<String> = None;
            for word in line.split(' ') {
                match word.split_once('=') {
                    Some((k, v)) if k.chars().all(|c| c.is_ascii_lowercase() || c == '_') => {
                        if let Some(done) = pending.take() {
                            map.insert(done, value.clone());
                        }
                        value = v.to_string();
                        pending = Some(k.to_string());
                    }
                    _ => {
                        value.push(' ');
                        value.push_str(word);
                    }
                }
            }
            if let Some(done) = pending {
                map.insert(done, value);
            }
            map
        })
        .collect()
}
