# SSIX version 1

A source catalog is published as `.safesync/index-GENERATION.ssi`. Files are
immutable: create a new generation instead of editing or truncating an existing
file. Readers map the file read-only. JSONL remains readable during migration.

All integers are little-endian. Every region begins at an 8-byte boundary.
The writer fills alignment gaps with zeroes. Offsets are absolute unless noted.
Records use explicit byte decoding; no native struct layout or alignment is assumed.

## Fixed header (256 bytes)

| Offset | Type | Meaning |
| --- | --- | --- |
| 0 | 4 bytes | `SSIX` |
| 4 | u32 | Format version, 1 |
| 8 | u32 | Fixed header length, 256 |
| 12 | u32 | Flags: bit 0 all files fingerprinted, bit 1 offline copy |
| 16 | u64 | Device ID; 0 only when an empty legacy index had no recorded device |
| 24 | u64 | Source root file ID |
| 32, 40 | u64 each | Scan start and finish, Unix seconds |
| 48 | u64 | Reused fingerprints |
| 56, 64, 72 | u64 each | Skipped symlinks, special files, nested mounts |
| 80, 88, 96 | u64 each | Files, content bytes, fingerprinted files |
| 104–199 | six (u64, u64) pairs | Region offset and byte length, in order below |
| 200–247 | six (u32, u32) pairs | Header heap offset and byte length for each field below |
| 248 | u32 | Drive role: 1 means source; other values are refused |
| 252 | u32 | Metadata schema, 1 |

The six regions are: header heap, records, path heap, fingerprints, fingerprint
index, trailer. Each starts at the aligned end of the previous region, including
zero-length regions. The trailer ends exactly at EOF. Counts, lengths and offsets
are checked for overflow, overlap, alignment and bounds before use.

The six header heap fields are generation, volume UUID, volume name, filesystem,
recorded absolute root, and exclusions. All are UTF-8 except the raw root bytes.
Exclusions are consecutive u32 byte lengths followed by UTF-8 bytes, allowing an
empty list or empty strings. The header heap is capped at 16 MiB. Fields are
contiguous; unused bytes, invalid UTF-8 and out-of-bounds references are refused.

## Records (56 bytes each)

Records are sorted strictly by raw relative path bytes. Duplicates are invalid.

| Offset | Type | Meaning |
| --- | --- | --- |
| 0, 8 | u64 each | File ID, size |
| 16 | i64 | mtime seconds |
| 24 | u32 | mtime nanoseconds, below 1,000,000,000 |
| 28 | i64 | ctime seconds |
| 36 | u32 | ctime nanoseconds, below 1,000,000,000 |
| 40 | u64 | Offset within path heap |
| 48 | u32 | Path byte length, excluding NUL |
| 52 | u32 | Flags: bit 0 has fingerprint |

The path heap contains each relative path followed by exactly one NUL, in record
order. Absolute paths, empty paths, NUL in a path, `.`/`..`, noncanonical components,
duplicates and unreferenced bytes are refused. Tabs, newlines and non-UTF-8 names
are preserved. Device ID is inherited from the header; a scan cannot cross mounts.

## Fingerprints and commit trailer

If any record is fingerprinted, the fingerprint region contains 32 raw SHA-256
bytes per record. Unhashed entries contain zero bytes and have their flag clear.
With no fingerprints the region is empty. A zero digest with its flag set is valid.

The fingerprint index contains one u32 record number per fingerprinted file,
sorted by `(fingerprint bytes, record number)`. Every number must refer to a hashed
record exactly once. This supports binary search for content under another name.
Version 1 permits at most `u32::MAX` records.

The 36-byte trailer contains SHA-256 over all preceding bytes (including finalized
header, heaps and padding), followed by `SSIX`. Publication flushes the complete
file, performs the macOS full durability flush, creates an exclusive hard link,
and syncs the containing directory, as for JSONL. Temporary files are removed.

Summary reads inspect the header, bounded header heap and complete trailer only.
They do **not** establish checksum or record validity. The first entry access
validates the digest, all paths, sorting, fingerprints and totals in one linear
pass. A mapping caches that result. `paths` then streams root/path/NUL through a
64 KiB buffer without creating a second inventory or full export in memory.

## Compatibility

`scan` and `sync` publish only source `.ssi` files and local offline copies.
`info`, `compare`, `lookup`, discovery and fingerprint reuse accept both codecs.
The sync engine currently adapts binary records into its in-memory planner model;
lookup and path export read mapped records directly. The drives screen reads
summaries only and hands search to `fzf`.

`safesync migrate [DRIVE|INDEX ...]` converts the selected newest source generations
and matching mounted/local copies without rescanning. With no arguments it starts
from the newest saved source per UUID. Original JSONL files are retained; existing
outputs are verified rather than overwritten. Legacy backup/scratch inventories
are excluded from normal source searches. Retiring those historical inventories
into offline volume records remains a separate migration task.
