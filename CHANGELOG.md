# Changelog

Notable changes to safesync. Dates are the day the change landed on `main`.

## Unreleased

### Changed

- Scans and syncs started from the drives screen now run in a panel under
  the drive table instead of switching to a separate full-screen view. The
  table stays visible, the preview and confirmation happen in the panel, and
  dismissing the finished panel reloads the list.

### Fixed

- The review list rebuilt a row for every planned file on each keypress, so
  scrolling a plan with tens of thousands of files lagged behind the arrow
  keys. Only the rows on screen are built now.
- Finder's `.DS_Store` files, AppleDouble `._*` sidecars, `Thumbs.db`,
  `desktop.ini` and nested system folders (`.Spotlight-V100`, `.fseventsd`,
  `.Trashes`, …) were indexed and copied like media. Scans now skip them in
  every folder, and the index header records the rule.
- A scan on a large drive appeared to hang after the walk: a silent second
  pass reopened every file to check nothing had changed, showing no progress,
  and Esc could not stop it because scans ignored the cancel flag. The
  per-file pass is gone; only directories are rechecked, with progress shown,
  and Esc now stops a scan between directories or files without publishing.
- The screen now shows when an index is being written, and a sync logs the
  same step, so the moments after a walk are no longer blank.
- Sync now previews required and available space before refusing an oversized
  plan, and checks again at confirmation. A disk-full error stops subsequent
  actions after cleanup/rollback, reports an incomplete run, and preserves
  completed copies and history for a later retry.

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
