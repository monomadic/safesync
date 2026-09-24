# safesync

One-way media sync from an indexed source to a live-checked backup, with offline
lookup and a full-screen progress view. Built for Tower → Tower Backup, and for
pulling a selection off both onto an SSD at the speed of two drives.

## Build and install

Requires macOS and a Rust toolchain supporting edition 2024.

```sh
git clone https://github.com/monomadic/safesync.git
cd safesync
cargo build --release --locked
mkdir -p "$HOME/.local/bin"
install -m 755 target/release/safesync "$HOME/.local/bin/safesync"
```

The [dotfiles installer](https://github.com/monomadic/config/blob/master/scripts/install/install-safesync.sh)
clones or updates this repository at `$SRC_PATH/safesync` (default
`~/src/safesync`) and installs to `~/.local/bin`. Run
`scripts/install/install-safesync.sh` from the dotfiles checkout to use it.

## The drives screen

```sh
safesync                 # same as `safesync drives`
safesync --no-icons      # also accepts `safesync drives --no-icons`
```

Drives are grouped by source volume, with the source first and its backups
underneath. Groups use the sentinel's UUID links, never matching names.
Unassigned drives, scratch drives, and drives needing attention have separate
sections; the ignored system volume starts collapsed. Each row shows its role,
index status and free/capacity figures. Indexes being read show “Loading…”;
pending comparisons show “Loading comparison…” rather than a missing-index warning.
The selected drive's summary shows its
relationship, index coverage and location. From the table:

- `↑↓` / `j k` selects a drive or section; `←→` / `h l` collapses or expands
  its section. Enter toggles a section or opens a drive's index details.
- `d` opens scrollable index details (UUID, generations, scan exclusions and
  fingerprints reused); `?` opens help and inventory warnings. Esc returns.
- `y` **Check / sync** checks the selected backup against its linked source,
  previews the changes, and asks for confirmation before copying. No manual
  indexing is required first. It also works from the source or group heading
  when there is one linked backup; with several, select the intended backup.
  Both drives must be mounted. After syncing or cancelling, the drive list returns.
- `r` assigns a role to an unmarked disk (a backup then picks the mounted
  source it mirrors). Roles are never *changed* from here; that stays a
  deliberate edit of `drive.toml`.
- `s` indexes the selected source (`S` also fingerprints files that have none),
  then returns to the table.
- `/` hands source paths to `fzf --read0 --print0 --exact` after restoring the
  terminal. It searches the selected source (or its backup's source), including
  offline catalogs. A selection focuses its source row; `o` reveals it in Finder
  only after checking that the same source UUID is mounted. Requires `fzf`.
- `R` reloads volumes and indexes, keeping the selected drive by UUID.

The table reads index summaries only. Backups show **Check with y**; saved
inventories are never compared as evidence of a current backup. Persistent
check timestamps and offline records for backups without old indexes remain
planned. Use explicit `compare` for historical catalog comparisons.

The interface uses tagform's synthwave colours: a pink safesync badge, purple
header and shortcut bars, and lavender text over the terminal's own background.
Icons have extra trailing space for wide glyphs. Icons are enabled by default:

- 􀤂 — ordinary drive, also used for offline records whose current state is unknown.
- 􁘧 — needs attention: invalid configuration, missing index, pending backup
  changes, read-only volume, missing backup, or insufficient space.
- 􀩎 — an unassigned drive that can be given a role.

The preview shows required and available space; confirmation checks free space
again and refuses an oversized plan before transferring. Renames need no copy space;
replacements need space for the new content because the old version is kept
in history. `--no-icons` removes drive symbols; piped output is plain text.

Drives in a drawer remain in their group using display metadata recorded in
source catalogs or legacy inventory headers. Older indexes without role metadata
may show under “Offline drives · role unknown”; they can still be inspected
explicitly, but are excluded from normal source searches. Saved roles never authorize writes: live sentinels still
control every operation.

Without a TTY it prints the table as plain text.

## The sentinel

Every participating drive carries `ROOT/.safesync/drive.toml`. It is both the
drive's config and the thing that stops a copy in the wrong direction:

```toml
role = "backup"          # source | backup | scratch
name = "Tower Backup"
volume_uuid = "215CF628-…"   # pinned to this disk; a copied sentinel is refused
source_uuid = "695E64CC-…"   # backup only: the one source it mirrors
extras = "keep"          # keep | history — backup files the source no longer has
```

- **source** — never written. `sync` refuses to run towards it, `fill` refuses to
  write onto it.
- **backup** — written only by `sync`, and only from the source whose UUID it names.
- **scratch** — a working disk `fill` may copy onto.

```sh
safesync init /Volumes/Tower --role source
safesync init "/Volumes/Tower Backup" --role backup --source /Volumes/Tower
safesync show /Volumes/Tower
```

The source sentinel may contain `exclude = ["relative/folder", ...]`; these
literal subtrees are left out of both the source catalog and the backup check.
System folders (`.Trashes`, `.Spotlight-V100`, `.fseventsd`, `.rclone`, etc.)
are always excluded. New backup and scratch sentinels have no exclusions;
legacy values are ignored. `sync --exclude` is no longer supported.

## The index

`ROOT/.safesync/index-GENERATION.ssi` is the source drive's catalog: every
regular file with size, nanosecond mtime, file ID and — once known — a SHA-256
fingerprint. The last three generations are kept on the drive, and a copy of
each lands in `~/Library/Application Support/safesync/manifests/` so you can
search a source that is in a drawer. Backups and scratch drives do not publish
indexes. Existing historical indexes remain readable until explicit migration.
Offline records for backups without historical indexes are still planned.

The [binary format](BINARY_FORMAT.md) stores a fixed header, sorted 56-byte
records, raw NUL-terminated paths, fingerprints and a SHA-256 commit trailer.
Readers use a read-only mapping. Summary reads check structure and the complete
trailer without traversing entries; the first entry access validates the checksum,
paths and sortedness. The drives screen never builds a full search catalog.
JSONL files remain readable by `info`, `compare`, `lookup` and fingerprint reuse.

```sh
safesync scan /Volumes/Tower           # metadata; carries over known fingerprints
safesync scan /Volumes/Tower --hash    # also reads files that have no fingerprint yet
safesync scan /Volumes/Tower --hash --rehash   # audit: read everything again
```

A scan walks first and reads afterwards. The walk shows a bar against the
previous index's file count; fingerprinting then has an exact total, so it
shows bytes done, throughput and time left, updating within each file.

A fingerprint is reused when the file ID, size and mtime are unchanged, so a
renamed video is not read again. `sync` fingerprints everything it copies (it
read the bytes anyway) and saves those fingerprints in the source index.
Files already present on the backup are checked by size and mtime. What reuse cannot see is an in-place edit
that keeps size and mtime, or rot at rest — `--rehash` occasionally is the check.

## sync

```sh
safesync sync /Volumes/Tower "/Volumes/Tower Backup" [--verify] [--hash] [--yes]
```

Refreshes the source index and walks backup metadata in parallel, shows what
it would do, and waits for Enter. The source catalog is saved even if copying
is declined. `--hash` / `--rehash` control source fingerprinting; the backup
never uses a saved index or a fingerprint cache:

- **copy** — on the source only.
- **rename** — the backup already holds the content under another name (same
  size and mtime, or same fingerprint); moved, not copied. Only unambiguous
  one-to-one matches qualify. Backup candidates without a stamp match are read
  only if a missing source file has a matching size and a known fingerprint.
  Progress is shown during these reads. Fingerprint-based renames adopt the
  source mtime, so subsequent checks recognize them by metadata.
- **replace** — same path, different content. The backup's version goes to
  `.safesync/history/GENERATION/` first, never deleted.
- **retire** — only with `extras = "history"`: backup-only files move to history.

The source is never written except for its own `.safesync/index`. Each file is
copied uncached (`F_NOCACHE`), preallocated, read and written on separate
threads, hashed on the way through, given the source's mtime and xattrs, and
published under its real name with `renameatx_np(RENAME_EXCL)` — a name that is
already taken is an error, never an overwrite. Media paths are resolved through
opened directory handles: symlinked parents and nested mounts are refused for
copies, renames and history moves. Planned renames recheck both files against
the scan before moving. If a replacement fails, the previous file is restored. `--verify` reads each file back
after the copy. Esc stops after the current file. Newly learned fingerprints
are published to the source index at the end. Backup observations stay in
memory; the next check walks the backup again.

If the destination fills during sync, the failed partial copy is removed and a
failed replacement is restored from history. If restoration itself fails, the
error identifies that the previous version remains in history. No further actions
are started. Completed files stay, the run reports incomplete (exit 1), and a new
run checks the drives again to plan the remainder. History is never pruned to make
space automatically.

## fill

```sh
safesync fill --from /Volumes/Tower --from "/Volumes/Tower Backup" --to /Volumes/SSD/dump clips/2026
fd -0 . /Volumes/Tower/clips | safesync fill --from /Volumes/Tower --from "/Volumes/Tower Backup" --to /Volumes/SSD/dump --stdin
```

Reads source indexes to select files and checks optional backup readers live
at the same relative paths by size and nanosecond mtime. A mounted source must
be included in `--from`; backup-only files are never added to the selection.
Backups need no index. One copy worker runs per drive: files only one drive
has go first, shared files go to whichever drive frees up. The
destination must be a scratch drive or an unmarked disk; a source or backup is
refused. Files already at the destination with the same size and mtime are skipped.

## Offline lookup

```sh
safesync lookup --name 'Holiday.mov'
safesync lookup --file ~/Downloads/clip.mov       # same content under any name
safesync compare older-source.jsonl newer-source.ssi
safesync manifests
```

Results describe what the index recorded at scan time, not what is on the disk
right now. Exit 0 match, 1 no match, 3 same-size candidates without fingerprints.

## Path export and conversion

```sh
safesync paths Tower | fzf --read0 --exact
safesync paths | xargs -0 your-command
safesync migrate /Volumes/Tower
safesync migrate saved-source.jsonl
```

`paths [DRIVE|INDEX ...]` writes only full paths followed by NUL. With no arguments
it selects the newest saved source catalog per UUID. It preserves raw bytes,
including tabs, newlines and non-UTF-8 names, through a bounded output buffer.
Mounted source roots are matched by UUID; offline paths use the recorded root
and do not establish current existence or ownership. Use UUIDs or index filenames
when source names are ambiguous.

`migrate` converts selected source generations and matching local/mounted copies
without reading media. With no arguments it starts from the newest saved sources.
It retains JSONL originals and never overwrites an existing index. Normal retention
keeps three generations across both formats. Explicit retirement of historical
backup/scratch inventories into offline volume records remains planned.

## Non-interactive use

Without a TTY every command prints plain lines and `sync`/`fill` need `--yes`.
Exit 0 done, 1 cancelled or some files failed, 2 refused or errored.

## Tests

```sh
cargo test --locked
```

`tests/sync.rs` creates APFS ram disks with `hdiutil`/`diskutil` so sentinel
UUIDs, preallocation and exclusive renames are the real thing (about a minute).
