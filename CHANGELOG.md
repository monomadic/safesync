# Changelog

Notable changes to safesync. Dates are the day the change landed on `main`.

## Unreleased

### Fixed

- `fill` created its destination directory before checking the drive's
  sentinel, so a destination under a source or backup gained an empty
  directory before being refused. The role is now decided from the nearest
  existing ancestor before anything is created, and checked again afterwards.

### Changed

- TODO reorganised around the first real Tower sync: blockers listed first,
  the migration test backlog split into its own checklist, and new items for
  the scan recheck pass and for building the hash cache and planner from
  mapped binary records instead of a full parse.
- Documented test suite timing as about a minute rather than 25 seconds.

## 2026-09-25

### Added

- Binary source indexes (`.ssi`, format documented in `BINARY_FORMAT.md`):
  immutable, mmap'd, sorted by path, with a fingerprint index for content
  lookup and a committed trailer. `safesync migrate` converts saved JSONL
  catalogs without rescanning; JSONL stays readable.
- `safesync paths` streams full NUL-terminated source paths for `fzf`,
  `xargs -0` and `rg -z`; the drives screen hands search to `fzf` with `/`.
- Live backup checks: **y Check / sync** on backup rows, source rows and group
  headings, without indexing the backup first.
- Standalone build, install and development notes (`AGENTS.md`, README).

### Changed

- Backups and scratch drives no longer publish indexes. The source index is
  the only catalog; a backup is walked live and compared per file.
- The source sentinel's `exclude` list is the only exclusion list; legacy
  backup and scratch exclusions are accepted but ignored.
- The drives screen opens on index summaries (header and trailer) instead of
  parsing every index, and distinguishes a comparison still loading from a
  missing index.

## 2026-09-24

### Added

- Drives screen: roles, index state, role assignment for unmarked disks, scan
  hand-off and offline search.
- Scan progress: a walk bar sized from the previous index and an exact
  fingerprint bar with an ETA.

## 2026-09-23

### Changed

- Rebuilt around sentinels, indexed sync, parallel fill and a full-screen TUI.
  The earlier journal, lease and relationship layer was removed on purpose;
  see git history before this date for that version.
