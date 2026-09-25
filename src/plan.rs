//! Plan from a source catalog and a transient live backup observation.
use crate::{
    drive::Extras,
    filesystem::Stamp,
    manifest::{Entry, Manifest},
};
use anyhow::Result;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// On the source only.
    Copy { path: PathBuf, size: u64 },
    /// Both sides have the path with different content; the backup's version
    /// goes to history first.
    Replace { path: PathBuf, size: u64 },
    /// The backup already holds this content under the source's old name.
    Rename {
        from: PathBuf,
        to: PathBuf,
        size: u64,
    },
    /// On the backup only, and the sentinel says such files go to history.
    Retire { path: PathBuf, size: u64 },
    /// A directory a removal or rename may have emptied: dropped once only
    /// housekeeping files (.DS_Store, ._* sidecars) are left in it.
    Prune { path: PathBuf },
}
impl Action {
    pub fn path(&self) -> &PathBuf {
        match self {
            Self::Copy { path, .. }
            | Self::Replace { path, .. }
            | Self::Retire { path, .. }
            | Self::Prune { path } => path,
            Self::Rename { to, .. } => to,
        }
    }
    /// Bytes that have to cross the bus.
    pub fn transfer(&self) -> u64 {
        match self {
            Self::Copy { size, .. } | Self::Replace { size, .. } => *size,
            _ => 0,
        }
    }
}

#[derive(Debug, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub unchanged: usize,
    /// Backup-only files left alone because the sentinel says `extras = "keep"`.
    pub kept_extras: Vec<(PathBuf, u64)>,
}
impl Plan {
    pub fn transfer_bytes(&self) -> u64 {
        self.actions.iter().map(Action::transfer).sum()
    }
    pub fn renamed_bytes(&self) -> u64 {
        self.actions
            .iter()
            .map(|action| match action {
                Action::Rename { size, .. } => *size,
                _ => 0,
            })
            .sum()
    }
    pub fn count(&self, matches: impl Fn(&Action) -> bool) -> usize {
        self.actions.iter().filter(|action| matches(action)).count()
    }
}

// Fingerprints decide when both sides have one. Otherwise size and mtime do,
// as in rclone: copies keep their mtime, so an untouched pair agrees.
pub fn same_content(a: &Entry, b: &Entry) -> bool {
    if a.stamp.size != b.stamp.size {
        return false;
    }
    match (&a.sha256, &b.sha256) {
        (Some(a), Some(b)) => a == b,
        _ => {
            (a.stamp.mtime_seconds, a.stamp.mtime_nanos)
                == (b.stamp.mtime_seconds, b.stamp.mtime_nanos)
        }
    }
}

pub fn same_stamp(a: &Stamp, b: &Stamp) -> bool {
    (a.size, a.mtime_seconds, a.mtime_nanos) == (b.size, b.mtime_seconds, b.mtime_nanos)
}

/// Backup-only files with no stamp match among missing source paths, and a
/// size for which the source has a fingerprint. No other backup files need reading.
pub fn fingerprint_candidates(source: &Manifest, backup: &Manifest) -> Result<Vec<usize>> {
    let source_paths = source
        .entries
        .iter()
        .map(Entry::path)
        .collect::<Result<HashSet<_>>>()?;
    let backup_paths = backup
        .entries
        .iter()
        .map(Entry::path)
        .collect::<Result<HashSet<_>>>()?;
    let mut leftover_stamps = HashSet::new();
    for entry in &backup.entries {
        if !source_paths.contains(&entry.path()?) {
            leftover_stamps.insert(identity(entry));
        }
    }
    let mut stamps = HashSet::new();
    let mut sizes = HashSet::new();
    for entry in &source.entries {
        if !backup_paths.contains(&entry.path()?) {
            stamps.insert(identity(entry));
            if entry.sha256.is_some()
                && entry.stamp.size > 0
                && !leftover_stamps.contains(&identity(entry))
            {
                sizes.insert(entry.stamp.size);
            }
        }
    }
    let mut candidates = Vec::new();
    for (index, entry) in backup.entries.iter().enumerate() {
        if !source_paths.contains(&entry.path()?)
            && !stamps.contains(&identity(entry))
            && sizes.contains(&entry.stamp.size)
        {
            candidates.push(index);
        }
    }
    Ok(candidates)
}

// What a moved file is recognised by. A multi-gigabyte video sharing both its
// size and its nanosecond mtime with a different video does not happen.
type Identity = (u64, i64, i64);
fn identity(entry: &Entry) -> Identity {
    (
        entry.stamp.size,
        entry.stamp.mtime_seconds,
        entry.stamp.mtime_nanos,
    )
}

pub fn plan(source: &Manifest, backup: &Manifest, extras: Extras) -> Result<Plan> {
    let mut there: BTreeMap<PathBuf, &Entry> = BTreeMap::new();
    for entry in &backup.entries {
        there.insert(entry.path()?, entry);
    }
    let mut plan = Plan::default();
    let mut missing = Vec::new();
    for entry in &source.entries {
        let path = entry.path()?;
        match there.remove(&path) {
            Some(existing) if same_content(entry, existing) => plan.unchanged += 1,
            Some(_) => plan.actions.push(Action::Replace {
                path,
                size: entry.stamp.size,
            }),
            None => missing.push((path, entry)),
        }
    }

    // `there` now holds backup-only files: candidates for a rename. Only an
    // unambiguous pairing counts — one missing file, one leftover, same identity.
    let mut leftovers: HashMap<Identity, Vec<PathBuf>> = HashMap::new();
    for (path, entry) in &there {
        leftovers
            .entry(identity(entry))
            .or_default()
            .push(path.clone());
    }
    let mut wanted: HashMap<Identity, usize> = HashMap::new();
    for (_, entry) in &missing {
        *wanted.entry(identity(entry)).or_default() += 1;
    }
    let mut unresolved = Vec::new();
    for (path, entry) in missing {
        let key = identity(entry);
        let size = entry.stamp.size;
        let from = match leftovers.get(&key) {
            Some(paths) if paths.len() == 1 && wanted[&key] == 1 && size > 0 => {
                Some(paths[0].clone())
            }
            _ => None,
        };
        match from {
            Some(from) if same_content(entry, there[&from]) => {
                there.remove(&from);
                plan.actions.push(Action::Rename {
                    from,
                    to: path,
                    size,
                });
            }
            _ => unresolved.push((path, entry)),
        }
    }

    // A second pass pairs content across different timestamps. Keep stamp
    // ambiguities as copies: hashing must not silently weaken the first pass.
    let mut by_hash: HashMap<(u64, &str), Vec<PathBuf>> = HashMap::new();
    for (path, entry) in &there {
        if !wanted.contains_key(&identity(entry))
            && let Some(hash) = entry.sha256.as_deref()
        {
            by_hash
                .entry((entry.stamp.size, hash))
                .or_default()
                .push(path.clone());
        }
    }
    let mut wanted_hash: HashMap<(u64, &str), usize> = HashMap::new();
    for (_, entry) in &unresolved {
        if let Some(hash) = entry.sha256.as_deref() {
            *wanted_hash.entry((entry.stamp.size, hash)).or_default() += 1;
        }
    }
    for (path, entry) in unresolved {
        let size = entry.stamp.size;
        let from = entry.sha256.as_deref().and_then(|hash| {
            let key = (size, hash);
            let paths = by_hash.get(&key)?;
            (size > 0
                && !leftovers.contains_key(&identity(entry))
                && paths.len() == 1
                && wanted_hash[&key] == 1)
                .then(|| paths[0].clone())
        });
        if let Some(from) = from
            && there.remove(&from).is_some()
        {
            plan.actions.push(Action::Rename {
                from,
                to: path,
                size,
            });
            continue;
        }
        plan.actions.push(Action::Copy { path, size });
    }

    for (path, entry) in there {
        match extras {
            Extras::Keep => plan.kept_extras.push((path, entry.stamp.size)),
            Extras::History => plan.actions.push(Action::Retire {
                path,
                size: entry.stamp.size,
            }),
        }
    }
    // Every directory a rename or removal leaves behind is a prune
    // candidate, deepest first so a child goes before its parent.
    let mut candidates: Vec<PathBuf> = Vec::new();
    for action in &plan.actions {
        let left = match action {
            Action::Rename { from, .. } => from,
            Action::Retire { path, .. } => path,
            _ => continue,
        };
        let mut parent = left.parent();
        while let Some(dir) = parent.filter(|p| !p.as_os_str().is_empty()) {
            candidates.push(dir.to_path_buf());
            parent = dir.parent();
        }
    }
    candidates.sort_by(|a, b| {
        b.components()
            .count()
            .cmp(&a.components().count())
            .then_with(|| a.cmp(b))
    });
    candidates.dedup();
    plan.actions
        .extend(candidates.into_iter().map(|path| Action::Prune { path }));
    // Renames and removals first: they free names and space for the copies;
    // pruning follows them, before anything is written.
    plan.actions.sort_by_key(|action| match action {
        Action::Rename { .. } => 0,
        Action::Retire { .. } => 1,
        Action::Prune { .. } => 2,
        Action::Replace { .. } => 3,
        Action::Copy { .. } => 4,
    });
    Ok(plan)
}
