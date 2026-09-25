use crate::{
    filesystem::{self, Stamp, Volume},
    manifest::{Entry, Header, Manifest, Role, SCHEMA, encode_path, generation, now},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    fs::{self, File},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// How much reading a scan may do to obtain fingerprints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hashing {
    /// Metadata only.
    None,
    /// Carry over fingerprints already known; read nothing.
    Known,
    /// Carry over known fingerprints and read every file that lacks one.
    Missing,
}
impl From<bool> for Hashing {
    fn from(hash: bool) -> Self {
        if hash { Self::Missing } else { Self::None }
    }
}

pub struct Progress {
    pub files: usize,
    pub bytes: u64,
    pub reused: u64,
    /// Set once the walk is over and files are being read for fingerprints.
    pub hashing: Option<HashProgress>,
    /// Set during the final pass over directories: (checked, total).
    pub checking: Option<(usize, usize)>,
}

/// The second half of a `Hashing::Missing` scan. Totals are exact: the walk
/// has already found every file without a reusable fingerprint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HashProgress {
    pub files_done: usize,
    pub files_total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

// Fingerprints carried over from earlier scans of the same volume, keyed on
// (file ID, size, mtime). Keying on the file ID rather than the path means a
// renamed video is not read again. APFS never reissues a file ID, so a hit is
// the same file object; what this cannot see is an in-place edit that keeps
// the size and restores the mtime. `--rehash` is the audit for that.
pub struct HashCache {
    volume_uuid: String,
    // None marks a key that earlier scans disagree about: never reused.
    hashes: HashMap<(u64, u64, i64, i64), Option<String>>,
}
impl HashCache {
    pub fn new(volume: &Volume) -> Self {
        Self {
            volume_uuid: volume.uuid.clone(),
            hashes: HashMap::new(),
        }
    }
    fn key(stamp: &Stamp) -> (u64, u64, i64, i64) {
        (
            stamp.file_id,
            stamp.size,
            stamp.mtime_seconds,
            stamp.mtime_nanos,
        )
    }
    /// File IDs mean nothing on another volume, so foreign manifests add nothing.
    pub fn add(&mut self, manifest: &Manifest) {
        if manifest.header.volume.uuid != self.volume_uuid {
            return;
        }
        for entry in &manifest.entries {
            let Some(hash) = &entry.sha256 else { continue };
            self.hashes
                .entry(Self::key(&entry.stamp))
                .and_modify(|known| {
                    if known.as_ref() != Some(hash) {
                        *known = None;
                    }
                })
                .or_insert_with(|| Some(hash.clone()));
        }
    }
    /// Adds every readable manifest in a directory. The cache is an optimisation,
    /// so a damaged or foreign file is skipped and costs only a re-read.
    pub fn add_directory(&mut self, directory: &Path) {
        let Ok(listing) = fs::read_dir(directory) else {
            return;
        };
        for entry in listing.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_file())
                && crate::manifest::is_index(&path)
                && Manifest::summary(&path).is_ok_and(|s| s.header.volume.uuid == self.volume_uuid)
                && let Ok(manifest) = Manifest::load(&path)
            {
                self.add(&manifest);
            }
        }
    }
    pub fn len(&self) -> usize {
        self.hashes.values().flatten().count()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn get(&self, stamp: &Stamp) -> Option<&String> {
        self.hashes.get(&Self::key(stamp))?.as_ref()
    }
}

// No ignored errors, hidden-file rules, or incomplete manifest publication.
// The only implicit scope exclusions are our metadata, the operating
// systems' own housekeeping files (`SYSTEM_NAMES`, AppleDouble `._*`
// files), and non-regular objects.

/// Names that are never media, wherever they appear: Finder and Windows
/// folder metadata, and the per-volume service directories that a drive
/// formatted elsewhere or copied through a non-APFS filesystem carries
/// deep inside its tree, not only at its root.
const SYSTEM_NAMES: &[&str] = &[
    ".DS_Store",
    ".localized",
    ".apdisk",
    ".Spotlight-V100",
    ".fseventsd",
    ".Trashes",
    ".TemporaryItems",
    ".DocumentRevisions-V100",
    "Thumbs.db",
    "desktop.ini",
    "$RECYCLE.BIN",
    "System Volume Information",
];

/// Housekeeping the scan leaves out by name: our own metadata directory,
/// the system names above, and AppleDouble sidecars (`._name`, the resource
/// fork and Finder info macOS writes beside a file on filesystems without
/// native forks).
pub(crate) fn housekeeping(name: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let bytes = name.as_bytes();
    name == ".safesync"
        || bytes.starts_with(b"._")
        || SYSTEM_NAMES.iter().any(|s| s.as_bytes() == bytes)
}
pub fn scan(
    root: &Path,
    volume: Volume,
    hash: bool,
    progress: impl FnMut(Progress),
) -> Result<Manifest> {
    scan_with_exclusions(root, volume, hash, &[], progress)
}

pub fn scan_with_exclusions(
    root: &Path,
    volume: Volume,
    hash: bool,
    exclusions: &[PathBuf],
    progress: impl FnMut(Progress),
) -> Result<Manifest> {
    scan_with_reuse(root, volume, hash, exclusions, None, progress)
}

pub fn scan_with_reuse(
    root: &Path,
    volume: Volume,
    hashing: impl Into<Hashing>,
    exclusions: &[PathBuf],
    reuse: Option<&HashCache>,
    progress: impl FnMut(Progress),
) -> Result<Manifest> {
    scan_cancellable(root, volume, hashing, exclusions, reuse, None, progress)
}

/// Stops with `Cancelled` as soon as `cancel` is set: between directories
/// during the walk, between files while fingerprinting, and during the
/// closing directory check. Nothing is published either way.
pub fn scan_cancellable(
    root: &Path,
    volume: Volume,
    hashing: impl Into<Hashing>,
    exclusions: &[PathBuf],
    reuse: Option<&HashCache>,
    cancel: Option<&AtomicBool>,
    mut progress: impl FnMut(Progress),
) -> Result<Manifest> {
    let hashing = hashing.into();
    let check_cancel = || -> Result<()> {
        ensure!(
            !cancel.is_some_and(|c| c.load(Ordering::Relaxed)),
            "Cancelled"
        );
        Ok(())
    };
    ensure!(
        reuse.is_none_or(|cache| cache.volume_uuid == volume.uuid),
        "Hash cache belongs to another volume"
    );
    for excluded in exclusions {
        ensure!(
            !excluded.as_os_str().is_empty()
                && excluded
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "Exclusions must be relative paths without . or .."
        );
    }
    let root = root.canonicalize().context("Cannot resolve scan root")?;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&root)?;
    let root_metadata = directory.metadata()?;
    let started = now();
    let device = root_metadata.dev();
    let mut directories = vec![(PathBuf::new(), Stamp::of(&root_metadata))];
    let mut queue = vec![PathBuf::new()];
    let mut entries = Vec::new();
    let mut skipped_symlinks = 0;
    let mut skipped_special = 0;
    let mut skipped_mounts = 0;
    let mut bytes = 0;
    let mut reused = 0;
    let mut pending: Vec<usize> = Vec::new();
    while let Some(relative) = queue.pop() {
        check_cancel()?;
        let parent = if relative.as_os_str().is_empty() {
            directory.try_clone()?
        } else {
            filesystem::open_relative(&directory, &relative, device)?
        };
        for name in
            filesystem::names(&parent).with_context(|| format!("Cannot list {:?}", relative))?
        {
            if housekeeping(&name) {
                continue;
            }
            let child = relative.join(&name);
            if exclusions
                .iter()
                .any(|excluded| child.starts_with(excluded))
            {
                continue;
            }
            let (mode, child_device) = filesystem::child_mode(&parent, &name)?;
            if child_device != device {
                skipped_mounts += 1;
                continue;
            }
            if mode == libc::S_IFLNK as u32 {
                skipped_symlinks += 1;
                continue;
            }
            if mode != libc::S_IFDIR as u32 && mode != libc::S_IFREG as u32 {
                skipped_special += 1;
                continue;
            }
            let opened = filesystem::open_relative(&directory, &child, device)
                .with_context(|| format!("Cannot read {:?}", child))?;
            let metadata = opened.metadata()?;
            let stamp = Stamp::of(&metadata);
            if mode == libc::S_IFDIR as u32 {
                ensure!(metadata.is_dir(), "Entry type changed while scanning");
                directories.push((child.clone(), stamp));
                queue.push(child);
            } else {
                ensure!(metadata.is_file(), "Entry type changed while scanning");
                let known = reuse.and_then(|cache| cache.get(&stamp));
                let sha256 = if hashing == Hashing::None {
                    None
                } else if let Some(known) = known {
                    reused += 1;
                    Some(known.clone())
                } else {
                    if hashing == Hashing::Missing {
                        pending.push(entries.len());
                    }
                    None
                };
                bytes += stamp.size;
                entries.push(Entry {
                    path_base64: encode_path(&child),
                    stamp,
                    sha256,
                });
                progress(Progress {
                    files: entries.len(),
                    bytes,
                    reused,
                    hashing: None,
                    checking: None,
                });
            }
        }
    }
    let entries_count = entries.len();
    // Fingerprinting comes after the walk so its total is known up front: the
    // slow half of a scan can show a real bar, and a spinning disk sees a
    // metadata pass and then a sequential read pass instead of an alternation.
    let mut hash = HashProgress {
        files_total: pending.len(),
        bytes_total: pending.iter().map(|&i| entries[i].stamp.size).sum(),
        ..HashProgress::default()
    };
    for index in pending {
        check_cancel()?;
        let entry = &mut entries[index];
        let relative = entry.path()?;
        let mut opened = filesystem::open_relative(&directory, &relative, device)
            .with_context(|| format!("Cannot read {:?}", relative))?;
        let digest = filesystem::hash_file_with(&mut opened, &entry.stamp, |n| {
            hash.bytes_done += n;
            progress(Progress {
                files: entries_count,
                bytes,
                reused,
                hashing: Some(hash),
                checking: None,
            });
        })
        .with_context(|| format!("Cannot fingerprint {:?}", relative))?;
        entry.sha256 = Some(digest);
        hash.files_done += 1;
        progress(Progress {
            files: entries_count,
            bytes,
            reused,
            hashing: Some(hash),
            checking: None,
        });
    }
    // Catch changes during the walk rather than presenting a partial tree as a
    // complete observation. Directories are enough: any add, remove or rename
    // beneath one moves its mtime. Files are not reopened here — on a
    // 1.5 M-file drive that second pass cost more than the walk and showed no
    // progress — because every file's stamp is rechecked immediately before
    // it is read for a copy or a fingerprint. This is not a filesystem
    // snapshot or a write lock.
    let total = directories.len();
    for (checked, (relative, expected)) in directories.iter().enumerate() {
        check_cancel()?;
        let opened = if relative.as_os_str().is_empty() {
            directory.try_clone()?
        } else {
            filesystem::open_relative(&directory, relative, device)?
        };
        ensure!(
            Stamp::of(&opened.metadata()?) == *expected,
            "Directory changed during scan: {:?}; retry with a quiet source",
            relative
        );
        progress(Progress {
            files: entries_count,
            bytes,
            reused,
            hashing: None,
            checking: Some((checked + 1, total)),
        });
    }
    let current_root = File::open(&root)?.metadata()?;
    ensure!(
        current_root.dev() == device && current_root.ino() == root_metadata.ino(),
        "Scan root was replaced"
    );
    entries.sort_by(|a, b| a.path_base64.cmp(&b.path_base64));
    Ok(Manifest {
        header: Header {
            schema: SCHEMA,
            generation: generation(),
            role: Role::Inventory,
            volume,
            drive: None,
            root_base64: encode_path(&root),
            root_file_id: root_metadata.ino(),
            device: Some(device),
            started_unix: started,
            finished_unix: now(),
            hash_algorithm: "sha256".into(),
            content_hashed: hashing != Hashing::None
                && entries.iter().all(|entry| entry.sha256.is_some()),
            exclusions: vec![
                "Any directory or file named .safesync".into(),
                "AppleDouble ._* files and system names (.DS_Store, .Spotlight-V100, Thumbs.db, …)"
                    .into(),
                "Symlinks and special files".into(),
                "Other mounted filesystems".into(),
            ]
            .into_iter()
            .chain(
                exclusions
                    .iter()
                    .map(|p| format!("Relative subtree: {:?}", p)),
            )
            .collect(),
            skipped_symlinks,
            skipped_special,
            skipped_mounts,
            reused_hashes: reused,
        },
        entries,
    })
}
