//! Names: what a directory entry set for a name looks like.
//!
//! A name that is a valid 8.3 name in one case needs only a short entry, with
//! the Windows NT lower-case flags when it is lower case. Any other name is
//! stored as a run of long-name entries over a short entry holding a generated
//! 8.3 alias. The alias gets a `~N` tail when making it lost something
//! (characters dropped or replaced, parts cut to fit) or when the plain alias
//! is taken; a name that differs from its alias only in case needs no tail.

use crate::{Error, Result};

pub(crate) const NT_LOWER_BASE: u8 = 0x08;
pub(crate) const NT_LOWER_EXTENSION: u8 = 0x10;

/// The entries to store for a name.
pub(crate) struct NewName {
    /// The long name as UTF-16, when the name needs long-name entries.
    pub long: Option<Vec<u16>>,
    pub short: [u8; 11],
    pub nt_flags: u8,
}

pub(crate) fn checksum(short: &[u8; 11]) -> u8 {
    short
        .iter()
        .fold(0u8, |sum, byte| sum.rotate_right(1).wrapping_add(*byte))
}

/// Compares two names the way FAT does: without regard to case.
pub(crate) fn same_name(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

/// Checks a name and returns it as UTF-16 units.
pub(crate) fn validate(name: &str) -> Result<Vec<u16>> {
    if name.is_empty() {
        return Err(Error::InvalidName("the name is empty"));
    }
    if name == "." || name == ".." {
        return Err(Error::InvalidName(
            "'.' and '..' cannot be created or removed",
        ));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(Error::InvalidName("a name cannot end in a dot or a space"));
    }
    if name
        .chars()
        .any(|c| (c as u32) < 0x20 || "\"*/:<>?\\|".contains(c))
    {
        return Err(Error::InvalidName(
            "a name cannot hold control characters or any of \" * / : < > ? \\ |",
        ));
    }
    let units: Vec<u16> = name.encode_utf16().collect();
    if units.len() > 255 {
        return Err(Error::InvalidName("a name is at most 255 UTF-16 units"));
    }
    Ok(units)
}

fn legal_short(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"$%'-_@~`!(){}^#&".contains(&byte)
}

/// All letters in `part` are of one case. Returns whether it is lower case.
fn single_case(part: &str) -> Option<bool> {
    let lower = part.chars().any(|c| c.is_ascii_lowercase());
    let upper = part.chars().any(|c| c.is_ascii_uppercase());
    (!(lower && upper)).then_some(lower)
}

fn pad(base: &str, extension: &str) -> [u8; 11] {
    let mut short = [b' '; 11];
    short[..base.len()].copy_from_slice(base.as_bytes());
    short[8..8 + extension.len()].copy_from_slice(extension.as_bytes());
    short
}

/// Makes the entries for `name`. `taken` says whether a short name is used by
/// another entry of the directory.
pub(crate) fn make(name: &str, taken: impl Fn(&[u8; 11]) -> bool) -> Result<NewName> {
    let units = validate(name)?;
    // A name that is already 8.3 in one case.
    let (base, extension) = match name.rsplit_once('.') {
        Some((base, extension)) => (base, extension),
        None => (name, ""),
    };
    let direct = !base.is_empty()
        && base.len() <= 8
        && extension.len() <= 3
        && !base.contains('.')
        && base.bytes().chain(extension.bytes()).all(legal_short);
    if direct {
        if let (Some(base_lower), Some(extension_lower)) =
            (single_case(base), single_case(extension))
        {
            let short = pad(&base.to_ascii_uppercase(), &extension.to_ascii_uppercase());
            if !taken(&short) {
                let mut flags = 0;
                if base_lower {
                    flags |= NT_LOWER_BASE;
                }
                if extension_lower {
                    flags |= NT_LOWER_EXTENSION;
                }
                return Ok(NewName {
                    long: None,
                    short,
                    nt_flags: flags,
                });
            }
        }
    }
    // A long name over a generated alias.
    let mut lossy = false;
    let mut cleaned = String::new();
    let mut leading = true;
    for character in name.chars() {
        if character == ' ' || (leading && character == '.') {
            lossy = true;
        } else {
            leading = false;
            cleaned.push(character);
        }
    }
    let (base, extension) = match cleaned.rsplit_once('.') {
        Some((base, extension)) => (base.to_string(), extension.to_string()),
        None => (cleaned.clone(), String::new()),
    };
    let convert = |part: &str, lossy: &mut bool| -> String {
        let mut out = String::new();
        for character in part.chars() {
            if character == '.' {
                *lossy = true;
            } else if character.is_ascii() && legal_short(character as u8) {
                out.push(character.to_ascii_uppercase());
            } else {
                *lossy = true;
                out.push('_');
            }
        }
        out
    };
    let mut base = convert(&base, &mut lossy);
    let mut extension = convert(&extension, &mut lossy);
    if base.is_empty() {
        base.push('_');
        lossy = true;
    }
    if base.len() > 8 {
        base.truncate(8);
        lossy = true;
    }
    if extension.len() > 3 {
        extension.truncate(3);
        lossy = true;
    }
    if !lossy {
        let short = pad(&base, &extension);
        if !taken(&short) {
            return Ok(NewName {
                long: Some(units),
                short,
                nt_flags: 0,
            });
        }
    }
    for number in 1..=999_999u32 {
        let tail = format!("~{number}");
        let keep = 8 - tail.len();
        let candidate = format!("{}{tail}", &base[..base.len().min(keep)]);
        let short = pad(&candidate, &extension);
        if !taken(&short) {
            return Ok(NewName {
                long: Some(units),
                short,
                nt_flags: 0,
            });
        }
    }
    Err(Error::InvalidName(
        "no free short name is left for the name",
    ))
}

/// The long-name entries for `units` over a short name, in the order they are
/// stored (the last fragment first).
pub(crate) fn long_entries(units: &[u16], short: &[u8; 11]) -> Vec<[u8; 32]> {
    const POSITIONS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
    let check = checksum(short);
    let count = units.len().div_ceil(13);
    let mut entries = Vec::with_capacity(count);
    for index in (0..count).rev() {
        let mut entry = [0u8; 32];
        entry[0] = (index + 1) as u8 | if index + 1 == count { 0x40 } else { 0 };
        entry[11] = 0x0F;
        entry[13] = check;
        for (slot, position) in POSITIONS.iter().enumerate() {
            let at = index * 13 + slot;
            let word = match at.cmp(&units.len()) {
                std::cmp::Ordering::Less => units[at],
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 0xFFFF,
            };
            entry[*position..*position + 2].copy_from_slice(&word.to_le_bytes());
        }
        entries.push(entry);
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_with(name: &str, taken: &[&[u8; 11]]) -> NewName {
        make(name, |short| taken.contains(&short)).unwrap()
    }

    fn short(name: &NewName) -> String {
        String::from_utf8_lossy(&name.short).to_string()
    }

    #[test]
    fn a_name_that_is_already_8_3_needs_no_long_entries() {
        let name = make_with("README.TXT", &[]);
        assert!(name.long.is_none());
        assert_eq!(short(&name), "README  TXT");
        assert_eq!(name.nt_flags, 0);
        let name = make_with("readme.txt", &[]);
        assert!(name.long.is_none());
        assert_eq!(name.nt_flags, NT_LOWER_BASE | NT_LOWER_EXTENSION);
        let name = make_with("readme.TXT", &[]);
        assert_eq!(name.nt_flags, NT_LOWER_BASE);
        let name = make_with("NOEXT", &[]);
        assert_eq!(short(&name), "NOEXT      ");
    }

    #[test]
    fn mixed_case_keeps_the_exact_alias_without_a_tail() {
        let name = make_with("Readme.txt", &[]);
        assert_eq!(short(&name), "README  TXT");
        assert!(name.long.is_some());
        assert_eq!(name.nt_flags, 0);
    }

    #[test]
    fn a_lossy_alias_gets_a_numeric_tail() {
        assert_eq!(
            short(&make_with("a name with spaces.txt", &[])),
            "ANAMEW~1TXT"
        );
        assert_eq!(short(&make_with("verylongname.txt", &[])), "VERYLO~1TXT");
        assert_eq!(short(&make_with("file.text", &[])), "FILE~1  TEX");
        assert_eq!(short(&make_with(".hidden", &[])), "HIDDEN~1   ");
        assert_eq!(short(&make_with("a+b.txt", &[])), "A_B~1   TXT");
        assert_eq!(short(&make_with("two.dots.txt", &[])), "TWODOT~1TXT");
        assert_eq!(short(&make_with("naïve.txt", &[])), "NA_VE~1 TXT");
        assert_eq!(short(&make_with("\u{e5}x", &[])), "_X~1       ");
    }

    #[test]
    fn a_taken_alias_moves_to_the_next_number() {
        let first = *b"VERYLO~1TXT";
        let second = *b"VERYLO~2TXT";
        let name = make_with("verylongname.txt", &[&first, &second]);
        assert_eq!(short(&name), "VERYLO~3TXT");
        // A plain alias that is taken falls back to a tailed one.
        let readme = *b"README  TXT";
        let name = make_with("Readme.txt", &[&readme]);
        assert_eq!(short(&name), "README~1TXT");
        // The tail shortens the base as the number grows.
        let taken: Vec<[u8; 11]> = (1..=9).map(|n| pad(&format!("LONGNA~{n}"), "")).collect();
        let refs: Vec<&[u8; 11]> = taken.iter().collect();
        let name = make_with("longnamex", &refs);
        assert_eq!(short(&name), "LONGN~10   ");
    }

    #[test]
    fn names_that_cannot_be_stored_are_refused() {
        for bad in [
            "",
            ".",
            "..",
            "a.",
            "a ",
            "a/b",
            "a\\b",
            "a:b",
            "a*b",
            "a?b",
            "a\"b",
            "a<b",
            "a>b",
            "a|b",
            "tab\there",
        ] {
            assert!(validate(bad).is_err(), "{bad:?}");
        }
        assert!(validate(&"a".repeat(255)).is_ok());
        assert!(validate(&"a".repeat(256)).is_err());
        assert!(validate("日本語 name.txt").is_ok());
    }

    #[test]
    fn long_entries_carry_order_checksum_and_padding() {
        let short = *b"LONGNA~1TXT";
        let units: Vec<u16> = "a longer name.txt".encode_utf16().collect();
        let entries = long_entries(&units, &short);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0][0], 0x42, "last fragment first, flagged");
        assert_eq!(entries[1][0], 0x01);
        for entry in &entries {
            assert_eq!(entry[11], 0x0F);
            assert_eq!(entry[12], 0);
            assert_eq!(entry[13], checksum(&short));
            assert_eq!(&entry[26..28], &[0, 0]);
        }
        // 17 units: the second fragment holds 4, then a terminator, then padding.
        assert_eq!(&entries[0][1..3], &(b'.' as u16).to_le_bytes());
        assert_eq!(
            &entries[0][9..11],
            &[0, 0],
            "the fifth slot is the terminator"
        );
        assert_eq!(&entries[0][14..16], &[0xFF, 0xFF]);
        // An exact multiple of 13 has no terminator.
        let units: Vec<u16> = "abcdefghijklm".encode_utf16().collect();
        let entries = long_entries(&units, &short);
        assert_eq!(entries.len(), 1);
        assert_eq!(&entries[0][30..32], &(b'm' as u16).to_le_bytes());
    }

    #[test]
    fn names_compare_without_regard_to_case() {
        assert!(same_name("Readme.TXT", "README.txt"));
        assert!(same_name("ÄÖ", "äö"));
        assert!(!same_name("a", "b"));
    }

    #[test]
    fn the_checksum_matches_the_specification() {
        // Computed with the specification's pseudo-code, independently.
        assert_eq!(checksum(b"FOO     BAR"), 0x53);
        assert_eq!(checksum(b"LONGNA~1TXT"), 0xf4);
    }
}
