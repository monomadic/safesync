//! The model behind `safesync drives`: every mounted volume, its sentinel,
//! the state of its index, plus drives known only from the indexes saved on
//! this Mac. Everything here is read-only; the one write the screen can lead
//! to is `Drive::init` on an unmarked disk.
//!
//! Nothing in this module prints: it is drawn inside a raw-mode terminal, so
//! damaged indexes are collected into `warnings` instead of written to stderr.
use crate::{
    drive::{self, Drive, Extras, Role, Sentinel},
    engine::human,
    filesystem,
    manifest::{self, Manifest, RecordedDrive, Summary},
    plan,
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::mpsc,
};

/// What the sentinel check found on a volume.
#[derive(Clone, Debug)]
pub enum Marking {
    /// The boot volume never takes part.
    Boot,
    /// No stable volume UUID (network mount, some disk images): nothing to pin a sentinel to.
    NoIdentity,
    /// A plain disk with no `.safesync/drive.toml`. The only kind that can be assigned a role.
    Unmarked,
    Valid(Sentinel),
    /// The sentinel names another volume's UUID: it was copied here and every command refuses it.
    Copied(Sentinel),
    /// A sentinel file that does not parse.
    Invalid(String),
    /// Not mounted; known only from an index saved on this Mac.
    Offline,
}

#[derive(Clone, Debug)]
pub struct IndexStats {
    pub generation: String,
    pub files: usize,
    /// `None` until the full index has been read, for footers that predate the total.
    pub bytes: Option<u64>,
    pub started_unix: u64,
    pub finished_unix: u64,
    pub content_hashed: bool,
    pub reused_hashes: u64,
    pub skipped_symlinks: u64,
    pub skipped_special: u64,
    pub skipped_mounts: u64,
    pub exclusions: Vec<String>,
    /// Generations kept on the drive itself; 0 for an offline row.
    pub generations: usize,
    /// Whether `~/Library/Application Support/safesync/manifests` holds a copy.
    pub saved_locally: bool,
}

/// How a marked drive relates to the others.
#[derive(Clone, Debug, Default)]
pub enum Relation {
    #[default]
    None,
    Backup {
        source_name: Option<String>,
        source_online: bool,
        /// Files the source index has that the backup index lacks or holds at another size.
        behind: Option<usize>,
        /// Historical sync estimate, including renames and same-size changes.
        estimate: Option<SyncEstimate>,
    },
    Source {
        backups: Vec<String>,
    },
}

#[derive(Clone, Debug)]
pub struct SyncEstimate {
    pub actions: usize,
    pub transfer_bytes: u64,
}

impl SyncEstimate {
    pub fn from_indexes(
        source: &Manifest,
        backup: &Manifest,
        extras: Extras,
    ) -> anyhow::Result<Self> {
        let plan = plan::plan(source, backup, extras)?;
        Ok(Self {
            actions: plan.actions.len(),
            transfer_bytes: plan.transfer_bytes(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub name: String,
    /// Mount point; `None` for an offline drive.
    pub path: Option<PathBuf>,
    pub uuid: Option<String>,
    pub filesystem: String,
    pub total: u64,
    pub free: u64,
    pub writable: bool,
    pub marking: Marking,
    /// Display metadata from an offline index; never authorizes an operation.
    pub recorded: Option<RecordedDrive>,
    pub index: Option<IndexStats>,
    pub relation: Relation,
}

/// How a cell should be coloured; the screen maps these to its palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Err,
    Dim,
    Neutral,
}

impl Row {
    pub fn online(&self) -> bool {
        self.path.is_some()
    }
    pub fn sentinel(&self) -> Option<&Sentinel> {
        match &self.marking {
            Marking::Valid(s) => Some(s),
            _ => None,
        }
    }
    pub fn role(&self) -> Option<Role> {
        self.sentinel().map(|s| s.role).or_else(|| {
            if matches!(self.marking, Marking::Offline) {
                self.recorded.as_ref().map(|d| d.role)
            } else {
                None
            }
        })
    }
    pub fn source_uuid(&self) -> Option<&str> {
        self.sentinel()
            .and_then(|s| s.source_uuid.as_deref())
            .or_else(|| {
                if matches!(self.marking, Marking::Offline) {
                    self.recorded
                        .as_ref()
                        .and_then(|d| d.source_uuid.as_deref())
                } else {
                    None
                }
            })
    }
    pub fn role_text(&self) -> &'static str {
        match self.role() {
            Some(Role::Source) => "source",
            Some(Role::Backup) => "backup",
            Some(Role::Scratch) => "scratch",
            None => "—",
        }
    }
    /// Why `r` is refused on this row, or `None` when a role may be assigned.
    pub fn assign_refusal(&self) -> Option<&'static str> {
        match &self.marking {
            Marking::Unmarked if !self.writable => Some("this volume is read-only"),
            Marking::Unmarked => None,
            Marking::Boot => Some("the boot volume never takes part"),
            Marking::NoIdentity => Some("no stable volume UUID to pin a sentinel to"),
            Marking::Valid(_) => Some("already has a role; roles are not changed from here"),
            Marking::Copied(_) => {
                Some("carries a copied sentinel; remove .safesync/drive.toml deliberately first")
            }
            Marking::Invalid(_) => {
                Some("has an unreadable sentinel; fix or remove .safesync/drive.toml first")
            }
            Marking::Offline => Some("not mounted"),
        }
    }
    pub fn scan_refusal(&self) -> Option<&'static str> {
        match &self.marking {
            Marking::Valid(s) if s.role == Role::Source => None,
            Marking::Valid(_) => Some("only sources are indexed; use y to check a backup"),
            Marking::Unmarked => Some("no sentinel yet; assign a role first"),
            Marking::Offline => Some("not mounted"),
            _ => self.assign_refusal(),
        }
    }
    /// A capacity warning based on the saved sync plan, not a fresh scan.
    pub fn space_warning(&self) -> Option<String> {
        if !self.online() {
            return None;
        }
        if let Relation::Backup {
            estimate: Some(estimate),
            ..
        } = &self.relation
            && estimate.transfer_bytes > self.free
        {
            return Some(format!(
                "Not enough space: ~{} to copy, {} free (saved indexes)",
                human(estimate.transfer_bytes),
                human(self.free)
            ));
        }
        None
    }
    /// The STATE column.
    pub fn state(&self, now: u64) -> (String, Tone) {
        if let Some(warning) = self.space_warning() {
            return (warning, Tone::Warn);
        }
        match &self.marking {
            Marking::Boot => ("system, ignored".into(), Tone::Dim),
            Marking::NoIdentity => ("no volume UUID".into(), Tone::Dim),
            Marking::Unmarked if !self.writable => ("read-only volume".into(), Tone::Warn),
            Marking::Unmarked => ("assign a role to begin".into(), Tone::Neutral),
            Marking::Invalid(_) => ("invalid sentinel".into(), Tone::Err),
            Marking::Copied(_) => ("copied sentinel, refused".into(), Tone::Err),
            Marking::Offline => ("offline · saved index".into(), Tone::Dim),
            Marking::Valid(sentinel) => {
                if !self.writable {
                    return ("read-only volume".into(), Tone::Warn);
                }
                if sentinel.role == Role::Scratch {
                    return ("ready for fill".into(), Tone::Neutral);
                }
                let Some(index) = &self.index else {
                    return if sentinel.role == Role::Backup {
                        ("check / sync with y".into(), Tone::Neutral)
                    } else {
                        ("no index yet".into(), Tone::Warn)
                    };
                };
                if sentinel.role == Role::Backup {
                    return match &self.relation {
                        Relation::Backup {
                            behind: Some(0),
                            estimate: Some(estimate),
                            ..
                        } if estimate.actions > 0 => (
                            format!(
                                "{} pending changes (saved indexes)",
                                group(estimate.actions as u64)
                            ),
                            Tone::Warn,
                        ),
                        Relation::Backup {
                            behind: Some(0), ..
                        } => (
                            "0 missing or size-changed files (saved indexes)".into(),
                            Tone::Neutral,
                        ),
                        Relation::Backup {
                            behind: Some(n), ..
                        } => (
                            format!(
                                "{} missing or size-changed files (saved indexes)",
                                group(*n as u64)
                            ),
                            Tone::Warn,
                        ),
                        Relation::Backup {
                            source_name: None, ..
                        } => ("source unknown".into(), Tone::Warn),
                        _ => ("check / sync with y".into(), Tone::Neutral),
                    };
                }
                let age = now.saturating_sub(index.finished_unix);
                let tone = if age < 7 * 86_400 {
                    Tone::Ok
                } else {
                    Tone::Neutral
                };
                (format!("scanned {}", ago(index.finished_unix, now)), tone)
            }
        }
    }
}

pub struct Inventory {
    pub rows: Vec<Row>,
    pub loaded_unix: u64,
    pub warnings: Vec<String>,
}

/// A display group. The stable key uses a UUID, while the title follows the
/// source volume's current (or last recorded) name.
#[derive(Clone, Debug)]
pub struct Section {
    pub key: String,
    pub title: String,
    pub rows: Vec<usize>,
}

impl Inventory {
    pub fn sections(&self) -> Vec<Section> {
        let mut sources: Vec<String> = self
            .rows
            .iter()
            .filter_map(|row| match row.role() {
                Some(Role::Source) => row.uuid.clone(),
                Some(Role::Backup) => row.source_uuid().map(str::to_owned),
                _ => None,
            })
            .collect();
        sources.sort();
        sources.dedup();
        let source_row = |uuid: &str| {
            self.rows.iter().position(|r| {
                r.uuid.as_deref() == Some(uuid)
                    && (r.role() == Some(Role::Source)
                        || (matches!(r.marking, Marking::Offline) && r.role().is_none()))
            })
        };
        sources.sort_by_key(|uuid| {
            (
                source_row(uuid)
                    .map(|i| self.rows[i].name.to_lowercase())
                    .unwrap_or_default(),
                uuid.clone(),
            )
        });
        let mut sections = Vec::new();
        let mut used = HashSet::new();
        for uuid in sources {
            let source = source_row(&uuid);
            let mut members: Vec<usize> = self
                .rows
                .iter()
                .enumerate()
                .filter(|(_, r)| r.role() == Some(Role::Backup) && r.source_uuid() == Some(&uuid))
                .map(|(i, _)| i)
                .collect();
            members.sort_by_key(|i| self.rows[*i].name.to_lowercase());
            if let Some(i) = source {
                members.insert(0, i);
            }
            used.extend(members.iter().copied());
            let name = source
                .map(|i| self.rows[i].name.clone())
                .unwrap_or_else(|| format!("Unknown source ({uuid})"));
            sections.push(Section {
                key: format!("source:{uuid}"),
                title: format!("Sync group: {name}"),
                rows: members,
            });
        }
        for (key, title) in [
            ("attention", "Needs attention"),
            ("scratch", "Scratch drives"),
            ("unassigned", "Unassigned drives"),
            ("offline", "Offline drives · role unknown"),
            ("unsupported", "Unsupported volumes"),
            ("system", "System · ignored volumes"),
        ] {
            let mut members: Vec<usize> = self
                .rows
                .iter()
                .enumerate()
                .filter(|(i, row)| {
                    !used.contains(i)
                        && match key {
                            "scratch" => row.role() == Some(Role::Scratch),
                            "unassigned" => matches!(row.marking, Marking::Unmarked),
                            "offline" => matches!(row.marking, Marking::Offline),
                            "unsupported" => matches!(row.marking, Marking::NoIdentity),
                            "system" => matches!(row.marking, Marking::Boot),
                            _ => {
                                matches!(row.marking, Marking::Invalid(_) | Marking::Copied(_))
                                    || row.role() == Some(Role::Backup)
                            }
                        }
                })
                .map(|(i, _)| i)
                .collect();
            members.sort_by_key(|i| self.rows[*i].name.to_lowercase());
            if !members.is_empty() {
                used.extend(members.iter().copied());
                sections.push(Section {
                    key: key.into(),
                    title: title.into(),
                    rows: members,
                });
            }
        }
        sections
    }

    /// Load volume metadata and index summaries without reading entry tables.
    pub fn load() -> Self {
        let (mut inventory, pending) = Self::start();
        if let Ok(details) = pending.recv() {
            inventory.absorb(details);
        }
        inventory
    }

    /// The screen reads summaries only. Search and live backup checks are
    /// explicit handoffs; saved backup catalogs do not establish current state.
    pub fn start() -> (Self, mpsc::Receiver<Details>) {
        let now = manifest::now();
        let mut warnings = Vec::new();
        let library = library_summaries(&mut warnings);
        let mut rows = Vec::new();

        for mount in filesystem::mounts() {
            let uuid = mount.volume.as_ref().map(|v| v.uuid.clone());
            let name = mount
                .volume
                .as_ref()
                .map(|v| v.name.clone())
                .unwrap_or_else(|| {
                    mount
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "/".into())
                });
            let marking = if mount.boot {
                Marking::Boot
            } else {
                match (Sentinel::read(&mount.path), &mount.volume) {
                    (Err(error), _) => Marking::Invalid(format!("{error:#}")),
                    (Ok(None), None) => Marking::NoIdentity,
                    (Ok(None), Some(_)) => Marking::Unmarked,
                    (Ok(Some(sentinel)), None) => {
                        warnings.push(format!("{name}: sentinel on a volume with no UUID"));
                        Marking::Copied(sentinel)
                    }
                    (Ok(Some(sentinel)), Some(volume)) => {
                        if sentinel.volume_uuid == volume.uuid {
                            Marking::Valid(sentinel)
                        } else {
                            Marking::Copied(sentinel)
                        }
                    }
                }
            };
            let mut index = None;
            if let (Marking::Valid(sentinel), Some(volume)) = (&marking, &mount.volume) {
                let drive = Drive {
                    root: mount.path.clone(),
                    volume: volume.clone(),
                    sentinel: sentinel.clone(),
                };
                let generations = drive.generations();
                for path in generations.iter().rev() {
                    match Manifest::summary(path) {
                        Ok(s) if s.header.volume.uuid == volume.uuid => {
                            index = Some(stats(
                                &s,
                                generations.len(),
                                library.contains_key(&volume.uuid),
                            ));
                            break;
                        }
                        Ok(_) => warnings.push(format!("{name}: {path:?} indexes another volume")),
                        Err(error) => {
                            warnings.push(format!("{name}: damaged index {path:?}: {error:#}"))
                        }
                    }
                }
            }
            rows.push(Row {
                name,
                path: Some(mount.path),
                uuid,
                filesystem: mount.filesystem,
                total: mount.total,
                free: mount.free,
                writable: mount.writable,
                marking,
                recorded: None,
                index,
                relation: Relation::None,
            });
        }

        // Drives in a drawer: their newest saved index stands in for them.
        let mounted: HashSet<String> = rows.iter().filter_map(|r| r.uuid.clone()).collect();
        let mut offline: Vec<(String, (PathBuf, Summary))> = library
            .into_iter()
            .filter(|(uuid, _)| !mounted.contains(uuid))
            .collect();
        offline.sort_by(|a, b| a.1.1.header.volume.name.cmp(&b.1.1.header.volume.name));
        for (uuid, (_path, summary)) in offline {
            rows.push(Row {
                name: summary.header.volume.name.clone(),
                path: None,
                uuid: Some(uuid),
                filesystem: summary.header.volume.filesystem.clone(),
                total: 0,
                free: 0,
                writable: false,
                marking: Marking::Offline,
                recorded: summary.header.drive.clone(),
                index: Some(stats(&summary, 0, true)),
                relation: Relation::None,
            });
        }

        // Who mirrors whom needs no entries, so it is known before the thread
        // reports; `behind` and `estimate` arrive with the details.
        let by_uuid: HashMap<String, usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.uuid.clone().map(|u| (u, i)))
            .collect();
        for i in 0..rows.len() {
            rows[i].relation = match rows[i].role() {
                Some(Role::Backup) => {
                    let source = rows[i]
                        .source_uuid()
                        .and_then(|uuid| by_uuid.get(uuid).copied());
                    Relation::Backup {
                        source_name: source.map(|s| rows[s].name.clone()),
                        source_online: source.is_some_and(|s| rows[s].online()),
                        behind: None,
                        estimate: None,
                    }
                }
                Some(Role::Source) => Relation::Source {
                    backups: rows
                        .iter()
                        .filter(|r| {
                            r.source_uuid()
                                .is_some_and(|u| Some(u) == rows[i].uuid.as_deref())
                        })
                        .map(|r| r.name.clone())
                        .collect(),
                },
                Some(Role::Scratch) | None => Relation::None,
            };
        }

        let (sender, receiver) = mpsc::channel();
        // Entry tables are opened only by explicit search/compare/check commands.
        let _ = sender.send(Details {
            bytes: rows
                .iter()
                .map(|r| r.index.as_ref().and_then(|i| i.bytes))
                .collect(),
            comparisons: (0..rows.len()).map(|_| None).collect(),
            warnings: Vec::new(),
        });
        let inventory = Self {
            rows,
            loaded_unix: now,
            warnings,
        };
        (inventory, receiver)
    }

    /// Fold in optional detail updates. Initial loading supplies summaries only.
    pub fn absorb(&mut self, details: Details) {
        self.warnings.extend(details.warnings);
        for (row, bytes) in self.rows.iter_mut().zip(details.bytes) {
            if let (Some(index), Some(bytes)) = (&mut row.index, bytes) {
                index.bytes = Some(bytes);
            }
        }
        for (row, comparison) in self.rows.iter_mut().zip(details.comparisons) {
            if let (
                Relation::Backup {
                    behind, estimate, ..
                },
                Some(comparison),
            ) = (&mut row.relation, comparison)
            {
                *behind = Some(comparison.behind);
                *estimate = comparison.estimate;
            }
        }
    }

    /// Mounted, valid source drives: the choices when a backup picks what it mirrors.
    pub fn sources(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.online() && r.role() == Some(Role::Source))
            .map(|(i, _)| i)
            .collect()
    }

    /// Column headings and one row of cells per drive, for both the screen and plain output.
    pub fn table(&self) -> (Vec<&'static str>, Vec<Vec<String>>) {
        let headings = vec![
            "NAME", "ROLE", "FS", "SIZE", "FREE", "INDEX", "FILES", "BYTES", "STATE",
        ];
        let cells = self
            .rows
            .iter()
            .map(|row| {
                let (index_date, files, bytes) = match &row.index {
                    Some(i) => (date(i.finished_unix), group(i.files as u64), size(i.bytes)),
                    None => ("—".into(), "—".into(), "—".into()),
                };
                let (size, free) = if row.online() {
                    (human(row.total), human(row.free))
                } else {
                    ("—".into(), "—".into())
                };
                vec![
                    row.name.clone(),
                    row.role_text().into(),
                    row.filesystem.to_uppercase(),
                    size,
                    free,
                    index_date,
                    files,
                    bytes,
                    row.state(self.loaded_unix).0,
                ]
            })
            .collect();
        (headings, cells)
    }
}

/// A backup measured against its source's index.
pub struct Comparison {
    pub behind: usize,
    pub estimate: Option<SyncEstimate>,
}

/// Optional detail updates, indexed like `rows`. Initial loading does not
/// compute historical comparisons or read file inventories.
pub struct Details {
    /// Byte total per row, for footers written before it was recorded there.
    pub bytes: Vec<Option<u64>>,
    pub comparisons: Vec<Option<Comparison>>,
    pub warnings: Vec<String>,
}

fn stats(summary: &Summary, generations: usize, saved_locally: bool) -> IndexStats {
    let h = &summary.header;
    IndexStats {
        generation: h.generation.clone(),
        files: summary.files,
        bytes: summary.bytes,
        started_unix: h.started_unix,
        finished_unix: h.finished_unix,
        content_hashed: h.content_hashed,
        reused_hashes: h.reused_hashes,
        skipped_symlinks: h.skipped_symlinks,
        skipped_special: h.skipped_special,
        skipped_mounts: h.skipped_mounts,
        exclusions: h.exclusions.clone(),
        generations,
        saved_locally,
    }
}

/// The newest saved index per volume UUID, by its summary.
fn library_summaries(warnings: &mut Vec<String>) -> HashMap<String, (PathBuf, Summary)> {
    let mut newest: HashMap<String, (PathBuf, Summary)> = HashMap::new();
    let Ok(directory) = drive::library() else {
        return newest;
    };
    for path in drive::listing(&directory, |name| {
        crate::manifest::is_index(Path::new(name))
    }) {
        match Manifest::summary(&path) {
            Ok(summary) => {
                let uuid = summary.header.volume.uuid.clone();
                let replace = newest.get(&uuid).is_none_or(|(_, old)| {
                    old.header.finished_unix <= summary.header.finished_unix
                });
                if replace {
                    newest.insert(uuid, (path, summary));
                }
            }
            Err(error) => warnings.push(format!("saved index {path:?}: {error:#}")),
        }
    }
    newest
}

/// Files the source index has that the backup index lacks, or holds at another
/// size. A historical path/size comparison, not a sync plan or content check.
pub fn behind(source: &Manifest, backup: &Manifest) -> usize {
    let sizes: HashMap<&str, u64> = backup
        .entries
        .iter()
        .map(|e| (e.path_base64.as_str(), e.stamp.size))
        .collect();
    source
        .entries
        .iter()
        .filter(|e| sizes.get(e.path_base64.as_str()) != Some(&e.stamp.size))
        .count()
}

/// A byte total, or the placeholder shown while it is still being read.
pub fn size(bytes: Option<u64>) -> String {
    bytes.map(human).unwrap_or_else(|| "…".into())
}

/// Thousands separated by a space: `48 210`.
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

fn local(unix: u64) -> libc::tm {
    let t = unix as libc::time_t;
    // SAFETY: tm is plain data; localtime_r fills it and reads only `t`.
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm
}
pub fn date(unix: u64) -> String {
    let tm = local(unix);
    format!(
        "{:04}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    )
}
pub fn time(unix: u64) -> String {
    let tm = local(unix);
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}
pub fn ago(unix: u64, now: u64) -> String {
    let seconds = now.saturating_sub(unix);
    match seconds {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}
