### Added
- A single plausible entry in a cluster that nothing points to, with every other slot empty and no `.` and `..`, is reported as a salvage candidate with `FatDetail::lone_entry` set: the weakest evidence the driver reports (`Integrity::Unchecked`, `FoundBy::Content`). One entry among other non-empty bytes is still not reported. Scans stay resumable over any range.
