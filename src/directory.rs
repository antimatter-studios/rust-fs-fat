use crate::boot::FatKind;
use crate::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    pub name: String,
    pub attributes: u8,
    pub first_cluster: u32,
    pub size: u32,
    pub deleted: bool,
}

#[derive(Clone, Debug)]
pub struct DirectoryRecord {
    pub offset: usize,
    pub entry: DirectoryEntry,
    pub raw: [u8; 32],
}

pub fn parse_directory(bytes: &[u8], kind: FatKind) -> Result<Vec<DirectoryEntry>> {
    Ok(directory_records(bytes, kind, false)?
        .into_iter()
        .map(|record| record.entry)
        .collect())
}

pub fn scan_directory(bytes: &[u8], kind: FatKind) -> Result<Vec<DirectoryRecord>> {
    directory_records(bytes, kind, true)
}

fn directory_records(bytes: &[u8], kind: FatKind, salvage: bool) -> Result<Vec<DirectoryRecord>> {
    if !bytes.len().is_multiple_of(32) {
        return Err(Error::Invalid("directory data is not entry aligned"));
    }
    let mut entries = Vec::new();
    let mut lfn = Vec::<u16>::new();
    let mut lfn_expected = 0u8;
    let mut lfn_checksum = None;
    for (index, raw) in bytes.chunks_exact(32).enumerate() {
        if raw[0] == 0 {
            if !salvage {
                break;
            }
            lfn.clear();
            lfn_expected = 0;
            lfn_checksum = None;
            continue;
        }
        if raw[11] == 0x0F {
            let sequence = raw[0] & 0x1F;
            if sequence == 0 || sequence > 20 || raw[12] != 0 || raw[26] != 0 || raw[27] != 0 {
                lfn.clear();
                lfn_expected = 0;
                lfn_checksum = None;
                continue;
            }
            if raw[0] & 0x40 != 0 {
                lfn.clear();
                lfn.resize(usize::from(sequence) * 13, 0xFFFF);
                lfn_expected = sequence;
                lfn_checksum = Some(raw[13]);
            }
            if sequence != lfn_expected || lfn_checksum != Some(raw[13]) {
                lfn.clear();
                lfn_expected = 0;
                lfn_checksum = None;
                continue;
            }
            let offset = usize::from(sequence - 1) * 13;
            if lfn.len() >= offset + 13 {
                let positions = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
                for (index, pos) in positions.iter().enumerate() {
                    lfn[offset + index] = u16::from_le_bytes([raw[*pos], raw[*pos + 1]]);
                }
            }
            lfn_expected -= 1;
            continue;
        }
        let deleted = raw[0] == 0xE5;
        if salvage && !plausible_short_name(raw) {
            lfn.clear();
            lfn_expected = 0;
            lfn_checksum = None;
            continue;
        }
        let short = short_name(raw, deleted);
        let long = (lfn_expected == 0 && lfn_checksum == Some(short_checksum(&raw[..11])))
            .then(|| lfn_name(&lfn))
            .flatten();
        let name = long.unwrap_or(short);
        lfn.clear();
        lfn_expected = 0;
        lfn_checksum = None;
        if raw[11] & 0x08 != 0 || raw[11] & 0xC0 != 0 {
            continue;
        }
        let high = if kind == FatKind::Fat32 {
            u32::from(u16::from_le_bytes([raw[20], raw[21]])) & 0x0FFF
        } else {
            0
        };
        let low = u32::from(u16::from_le_bytes([raw[26], raw[27]]));
        entries.push(DirectoryRecord {
            offset: index * 32,
            raw: raw.try_into().unwrap(),
            entry: DirectoryEntry {
                name,
                attributes: raw[11],
                first_cluster: (high << 16) | low,
                size: u32::from_le_bytes(raw[28..32].try_into().unwrap()),
                deleted,
            },
        });
    }
    Ok(entries)
}

fn plausible_short_name(raw: &[u8]) -> bool {
    raw[11] & 0xC8 == 0
        && raw[12] & !0x18 == 0
        && raw[0] != b' '
        && raw[..11].iter().enumerate().all(|(index, byte)| {
            (index == 0 && matches!(*byte, 0xE5 | 0x05))
                || (*byte >= 0x20 && !b"\"*+,./:;<=>?[\\]|".contains(byte))
                || (*byte == b'.' && index < 2 && raw[11] & 0x10 != 0)
        })
}

fn short_checksum(name: &[u8]) -> u8 {
    name.iter()
        .fold(0u8, |sum, byte| sum.rotate_right(1).wrapping_add(*byte))
}

fn short_name(raw: &[u8], deleted: bool) -> String {
    let mut base = raw[..8].to_vec();
    let extension = &raw[8..11];
    if deleted {
        base[0] = b'_';
    }
    let mut base = String::from_utf8_lossy(&base).trim_end().to_string();
    let mut extension = String::from_utf8_lossy(extension).trim_end().to_string();
    // Byte 12 holds the Windows NT case flags: an 8.3 name stored in upper
    // case is shown in lower case where the flag for its part is set.
    if raw[12] & 0x08 != 0 {
        base.make_ascii_lowercase();
    }
    if raw[12] & 0x10 != 0 {
        extension.make_ascii_lowercase();
    }
    if extension.is_empty() {
        base
    } else {
        format!("{base}.{extension}")
    }
}

fn lfn_name(words: &[u16]) -> Option<String> {
    if words.is_empty() {
        return None;
    }
    let end = words
        .iter()
        .position(|word| *word == 0)
        .unwrap_or(words.len());
    let words = &words[..end];
    let name = String::from_utf16_lossy(words);
    (!name.is_empty() && !name.contains('\u{FFFD}')).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forensic_scan_keeps_deleted_entries_after_end_markers_and_skips_noise() {
        let mut bytes = vec![0; 160];
        bytes[..11].copy_from_slice(b"LIVE    TXT");
        bytes[11] = 0x20;
        bytes[64..75].copy_from_slice(b"\xE5OST    TXT");
        bytes[75] = 0x20;
        bytes[96..128].fill(0xFF);
        bytes[128..139].copy_from_slice(b"LABEL      ");
        bytes[139] = 0x08;
        for kind in [FatKind::Fat12, FatKind::Fat16, FatKind::Fat32] {
            let ordinary = parse_directory(&bytes, kind).unwrap();
            assert_eq!(ordinary.len(), 1);
            assert_eq!(ordinary[0].name, "LIVE.TXT");
            let forensic = scan_directory(&bytes, kind).unwrap();
            assert_eq!(forensic.len(), 2);
            assert_eq!(forensic[1].offset, 64);
            assert_eq!(forensic[1].entry.name, "_OST.TXT");
            assert!(forensic[1].entry.deleted);
            assert_eq!(forensic[1].raw[0], 0xE5);
        }
    }

    #[test]
    fn accepts_a_long_name_only_when_its_short_name_checksum_matches() {
        let short = *b"FOUND   TXT";
        let checksum = short_checksum(&short);
        let mut bytes = vec![0; 64];
        let lfn = &mut bytes[..32];
        lfn[0] = 0x41;
        lfn[11] = 0x0F;
        lfn[13] = checksum;
        let name: Vec<u16> = "Found.txt".encode_utf16().collect();
        let positions = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
        for (index, position) in positions.iter().enumerate() {
            let word =
                name.get(index)
                    .copied()
                    .unwrap_or(if index == name.len() { 0 } else { 0xFFFF });
            lfn[*position..*position + 2].copy_from_slice(&word.to_le_bytes());
        }
        let entry = &mut bytes[32..];
        entry[..11].copy_from_slice(&short);
        entry[11] = 0x20;
        assert_eq!(
            parse_directory(&bytes, FatKind::Fat16).unwrap()[0].name,
            "Found.txt"
        );

        bytes[13] ^= 1;
        assert_eq!(
            parse_directory(&bytes, FatKind::Fat16).unwrap()[0].name,
            "FOUND.TXT"
        );
    }

    #[test]
    fn nt_case_flags_lower_case_the_short_name_parts_they_cover() {
        let entry = |flags: u8| {
            let mut bytes = vec![0; 32];
            bytes[..11].copy_from_slice(b"README  TXT");
            bytes[11] = 0x20;
            bytes[12] = flags;
            parse_directory(&bytes, FatKind::Fat16)
                .unwrap()
                .remove(0)
                .name
        };
        assert_eq!(entry(0x00), "README.TXT");
        assert_eq!(entry(0x08), "readme.TXT");
        assert_eq!(entry(0x10), "README.txt");
        assert_eq!(entry(0x18), "readme.txt");
    }
}
