### Added
- A directory cluster that nothing points to and that has no `.` and `..` entries is recognised by its content (a run of plausible directory entries), and its entries are reported as salvage candidates with `FatDetail::found_by` set to `FoundBy::Content` and `Integrity::Unchecked`. File data that merely resembles one entry is not reported. Each cluster is judged on its own bytes, so scans stay resumable over any range.
- `FatDetail::found_by` says how the directory cluster holding a candidate was recognised: `Tree`, `DotEntries` or `Content`.
