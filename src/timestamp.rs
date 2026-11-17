//! FAT date and time fields.
//!
//! A FAT timestamp is a 16-bit date (years since 1980, month, day), a 16-bit
//! time with two-second resolution, and for creation times a count of tens of
//! milliseconds that adds up to two seconds more. It carries no time zone: the
//! values are local time as the writer saw it. This module reads and writes
//! them as UTC, which is what a caller with no zone to hand should expect, and
//! callers that keep local time convert before they ask.

/// The first and last year a FAT date can hold.
const FIRST_YEAR: i64 = 1980;
const LAST_YEAR: i64 = 2107;

/// A FAT date, time and tens-of-milliseconds count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FatTimestamp {
    pub date: u16,
    pub time: u16,
    /// 0 to 199 tens of milliseconds, added to `time` (creation time only).
    pub tenths: u8,
}

impl FatTimestamp {
    /// 1980-01-01 00:00:00, the earliest value a FAT date holds.
    pub const EPOCH: FatTimestamp = FatTimestamp {
        date: (1 << 5) | 1,
        time: 0,
        tenths: 0,
    };

    /// The current time, from the system clock.
    pub fn now() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Self::from_unix(now.as_secs() as i64, now.subsec_millis())
    }

    /// Converts seconds since 1970-01-01 (UTC) and a millisecond part. Times
    /// outside 1980 to 2107 are held at the nearest end of that range.
    pub fn from_unix(seconds: i64, millis: u32) -> Self {
        let days = seconds.div_euclid(86_400);
        let second_of_day = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        if year < FIRST_YEAR {
            return Self::EPOCH;
        }
        if year > LAST_YEAR {
            return Self {
                date: (((LAST_YEAR - FIRST_YEAR) as u16) << 9) | (12 << 5) | 31,
                time: (23 << 11) | (59 << 5) | 29,
                tenths: 0,
            };
        }
        let hour = (second_of_day / 3600) as u16;
        let minute = (second_of_day % 3600 / 60) as u16;
        let second = (second_of_day % 60) as u16;
        Self {
            date: (((year - FIRST_YEAR) as u16) << 9) | ((month as u16) << 5) | day as u16,
            time: (hour << 11) | (minute << 5) | (second / 2),
            tenths: ((second % 2) * 100) as u8 + (millis / 10).min(99) as u8,
        }
    }

    /// Seconds since 1970-01-01 (UTC) and the millisecond part, or `None` for a
    /// field that is not a real date or time (month 0, day 0, hour 31 ...).
    pub fn to_unix(self) -> Option<(i64, u32)> {
        let year = FIRST_YEAR + i64::from(self.date >> 9);
        let month = i64::from((self.date >> 5) & 0xF);
        let day = i64::from(self.date & 0x1F);
        let hour = i64::from(self.time >> 11);
        let minute = i64::from((self.time >> 5) & 0x3F);
        let second = i64::from(self.time & 0x1F) * 2;
        if !(1..=12).contains(&month) || day == 0 || hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        let days = days_from_civil(year, month, day);
        let extra = i64::from(self.tenths) * 10;
        Some((
            days * 86_400 + hour * 3600 + minute * 60 + second + extra / 1000,
            (extra % 1000) as u32,
        ))
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_known_instants_both_ways() {
        // 2000-02-29 12:34:56 UTC
        let stamp = FatTimestamp::from_unix(951_827_696, 0);
        assert_eq!(stamp.date, (20 << 9) | (2 << 5) | 29);
        assert_eq!(stamp.time, (12 << 11) | (34 << 5) | 28);
        assert_eq!(stamp.to_unix(), Some((951_827_696, 0)));
        // 1980-01-01 is the epoch of the format.
        assert_eq!(FatTimestamp::from_unix(315_532_800, 0), FatTimestamp::EPOCH);
    }

    #[test]
    fn an_odd_second_is_carried_in_the_tenths() {
        let stamp = FatTimestamp::from_unix(951_827_697, 250);
        assert_eq!(stamp.time & 0x1F, 28);
        assert_eq!(stamp.tenths, 125);
        assert_eq!(stamp.to_unix(), Some((951_827_697, 250)));
    }

    #[test]
    fn out_of_range_times_are_held_at_the_ends() {
        assert_eq!(FatTimestamp::from_unix(0, 0), FatTimestamp::EPOCH);
        assert_eq!(
            FatTimestamp::from_unix(-5_000_000_000, 0),
            FatTimestamp::EPOCH
        );
        let last = FatTimestamp::from_unix(10_000_000_000, 0);
        assert_eq!(last.date >> 9, 127);
        assert!(last.to_unix().is_some());
    }

    #[test]
    fn fields_that_are_not_a_date_have_no_instant() {
        let bad = FatTimestamp {
            date: 0,
            time: 0,
            tenths: 0,
        };
        assert_eq!(bad.to_unix(), None);
        let bad = FatTimestamp {
            date: FatTimestamp::EPOCH.date,
            time: 31 << 11,
            tenths: 0,
        };
        assert_eq!(bad.to_unix(), None);
    }
}
