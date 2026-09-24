# safesync

Standalone macOS-only Rust tool for indexed one-way drive sync, parallel fill,
and offline lookup. See README.md for behavior and TODO.md for planned work.

## Build and verify

- `cargo build --release --locked`
- `cargo test --locked`
- Install the release binary into `~/.local/bin`, never `~/.cargo/bin`.
- The dotfiles repository owns `scripts/install/install-safesync.sh`; it uses
  the shared Git source installer to keep this checkout current.
- Do not commit `target/`, binaries, credentials, or machine-private state.

## Safety model

**safesync is guarded by sentinels, not by care.** A drive takes part only if
it carries `.safesync/drive.toml` — `role = source | backup | scratch` pinned
to the volume UUID — and every command that writes media opens both sentinels
first: `sync` runs only source → the backup that names that source, `fill`
writes only onto scratch or unmarked disks, and a source is never written
except for its own index. The index (`.safesync/index-GENERATION.jsonl`) on
the drive is the source of truth; the copy in `~/Library/Application
Support/safesync/manifests/` is for `lookup` while the drive is unplugged.
Fingerprints are reused by file ID + size + mtime, so a rescan reads only new
files. Tests build real APFS ram disks (`tests/sync.rs`, ~25 s). It is meant
to replace `rclone-tower-safe`; until it has, the two coexist and neither
knows about the other's history directory. Don't add a journal, lease or
relationship layer back — that version was cut on purpose (git history before
2026-09-23).
