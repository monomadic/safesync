//! Source catalog selection and bounded NUL-delimited path export.
use crate::{
    binary,
    drive::{self, Drive, Role, Sentinel},
    filesystem,
    manifest::{self, Manifest, Summary},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Catalog {
    pub path: PathBuf,
    pub summary: Summary,
}
impl Catalog {
    pub fn open(path: PathBuf) -> Result<Self> {
        let summary = Manifest::summary(&path)?;
        ensure!(
            is_source(&summary),
            "{path:?} is a historical non-source inventory; it is not a source catalog"
        );
        Ok(Self { path, summary })
    }
}
pub fn is_source(summary: &Summary) -> bool {
    summary
        .header
        .drive
        .as_ref()
        .is_some_and(|d| d.role == Role::Source)
}

fn key(c: &Catalog) -> (u128, u64, u64, bool) {
    let mut pieces = c.summary.header.generation.split('-');
    (
        pieces
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(c.summary.header.finished_unix as u128 * 1_000_000_000),
        pieces.next().and_then(|s| s.parse().ok()).unwrap_or(0),
        pieces.next().and_then(|s| s.parse().ok()).unwrap_or(0),
        c.path.extension().is_some_and(|e| e == "ssi"),
    )
}
pub fn newest(catalogs: impl IntoIterator<Item = Catalog>) -> Vec<Catalog> {
    let mut by_uuid: BTreeMap<String, Catalog> = BTreeMap::new();
    for c in catalogs {
        let uuid = c.summary.header.volume.uuid.clone();
        if by_uuid.get(&uuid).is_none_or(|old| key(old) < key(&c)) {
            by_uuid.insert(uuid, c);
        }
    }
    by_uuid.into_values().collect()
}
pub fn saved() -> Result<Vec<Catalog>> {
    let mut catalogs = Vec::new();
    for path in drive::listing(&drive::library()?, |n| manifest::is_index(Path::new(n))) {
        // Keep damaged generations from blocking other sources. Explicitly named
        // indexes still report their errors directly through Catalog::open.
        match Manifest::summary(&path) {
            Ok(summary) if is_source(&summary) => catalogs.push(Catalog { path, summary }),
            Ok(_) => {}
            Err(error) => eprintln!("safesync: ignoring damaged catalog {path:?}: {error:#}"),
        }
    }
    Ok(newest(catalogs))
}

pub struct Selection {
    pub catalog: Catalog,
    pub root: PathBuf,
}

/// Resolve mounted roots by UUID, never by a reused mount-point name.
pub fn mounted_sources() -> BTreeMap<String, PathBuf> {
    filesystem::mounts()
        .into_iter()
        .filter_map(|m| {
            let volume = m.volume?;
            let sentinel = Sentinel::read(&m.path).ok()??;
            (sentinel.role == Role::Source && sentinel.volume_uuid == volume.uuid)
                .then_some((volume.uuid, m.path))
        })
        .collect()
}

pub fn select(inputs: &[PathBuf]) -> Result<Vec<Selection>> {
    let saved = saved()?;
    let mounted = mounted_sources();
    let mut available = saved.clone();
    if !inputs.is_empty() {
        for root in mounted.values() {
            let drive = Drive::open(root)?;
            for path in drive.generations().into_iter().rev() {
                if let Ok(catalog) = Catalog::open(path) {
                    if catalog.summary.header.volume.uuid == drive.volume.uuid {
                        available.push(catalog);
                        break;
                    }
                }
            }
        }
    }
    let available = newest(available);
    let mut chosen = Vec::new();
    if inputs.is_empty() {
        chosen = saved;
    }
    for input in inputs {
        if input.is_file() {
            chosen.push(Catalog::open(input.clone())?);
            continue;
        }
        if input.is_dir() {
            let drive = Drive::open(input)?;
            ensure!(
                drive.sentinel.role == Role::Source,
                "Only source drives export paths"
            );
            let catalog = drive
                .generations()
                .into_iter()
                .rev()
                .find_map(|p| {
                    Catalog::open(p)
                        .ok()
                        .filter(|c| c.summary.header.volume.uuid == drive.volume.uuid)
                })
                .context("Source has no readable catalog; scan it first")?;
            chosen.push(catalog);
            continue;
        }
        let matches: Vec<_> = available
            .iter()
            .filter(|c| {
                input.as_os_str() == std::ffi::OsStr::new(&c.summary.header.volume.name)
                    || input.as_os_str() == std::ffi::OsStr::new(&c.summary.header.volume.uuid)
            })
            .collect();
        ensure!(
            matches.len() == 1,
            "Drive {input:?} is missing or ambiguous; use its UUID or index path"
        );
        chosen.push(matches[0].clone());
    }
    resolve(newest(chosen), &mounted)
}
pub fn resolve(
    catalogs: Vec<Catalog>,
    mounted: &BTreeMap<String, PathBuf>,
) -> Result<Vec<Selection>> {
    catalogs
        .into_iter()
        .map(|catalog| {
            let root = mounted
                .get(&catalog.summary.header.volume.uuid)
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| manifest::decode_path(&catalog.summary.header.root_base64))?;
            Ok(Selection { catalog, root })
        })
        .collect()
}

pub fn write(selections: &[Selection], output: impl Write) -> Result<()> {
    let mut output = BufWriter::with_capacity(64 * 1024, output);
    for selection in selections {
        let file = File::open(&selection.catalog.path)?;
        if manifest::binary_file(&file)? {
            binary::Index::from_file(file)?.write_paths(&selection.root, &mut output)?;
        } else {
            let m = Manifest::load(&selection.catalog.path)?;
            for entry in &m.entries {
                output.write_all(selection.root.join(entry.path()?).as_os_str().as_bytes())?;
                output.write_all(&[0])?;
            }
        }
    }
    output.flush()?;
    Ok(())
}

/// Explicit, non-destructive conversion. Original JSONL inventories remain
/// readable until their normal generation retention removes them.
pub fn migrate(inputs: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let selections = select(inputs)?;
    let identities: std::collections::BTreeSet<_> = selections
        .iter()
        .map(|s| {
            (
                s.catalog.summary.header.volume.uuid.clone(),
                s.catalog.summary.header.generation.clone(),
            )
        })
        .collect();
    let mut catalogs: BTreeMap<PathBuf, Catalog> = selections
        .into_iter()
        .map(|s| (s.catalog.path.clone(), s.catalog))
        .collect();
    // Convert matching on-drive and local copies together when both are present.
    let mut candidates = drive::listing(&drive::library()?, |n| manifest::is_index(Path::new(n)));
    for root in mounted_sources().values() {
        candidates.extend(Drive::open(root)?.generations());
    }
    for path in candidates {
        if let Ok(c) = Catalog::open(path.clone()) {
            if identities.contains(&(
                c.summary.header.volume.uuid.clone(),
                c.summary.header.generation.clone(),
            )) {
                catalogs.insert(path, c);
            }
        }
    }
    let mut written = Vec::new();
    for catalog in catalogs.into_values() {
        let path = &catalog.path;
        if manifest::binary_file(&File::open(path)?)? {
            continue;
        }
        let output = path.with_extension("ssi");
        let mut m = Manifest::load(path)?;
        m.header.device = Some(
            m.header
                .device
                .or_else(|| m.entries.first().map(|e| e.stamp.device))
                .unwrap_or(0),
        );
        if output.exists() {
            let existing = Manifest::load(&output)?;
            let mut expected = m.entries.clone();
            expected.sort_by_key(|e| e.path().ok());
            ensure!(
                serde_json::to_value(&existing.header)? == serde_json::to_value(&m.header)?
                    && existing.entries == expected,
                "Existing binary catalog does not match {path:?}"
            );
        } else {
            check_migration_destination(&output, &m.header.volume.uuid)?;
            m.save_new(&output)?;
        }
        written.push(output);
    }
    Ok(written)
}

// Historical headers identify catalogs; live sentinels authorize writes on
// participating drives. Conversion may never become a backup/scratch writer.
fn check_migration_destination(output: &Path, uuid: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let parent = output.parent().unwrap_or(Path::new(".")).canonicalize()?;
    let device = std::fs::metadata(&parent)?.dev();
    for ancestor in parent.ancestors() {
        if std::fs::metadata(ancestor)?.dev() != device {
            break;
        }
        if ancestor
            .join(drive::METADATA_DIR)
            .join("drive.toml")
            .exists()
        {
            let drive = Drive::open(ancestor)?;
            ensure!(
                drive.sentinel.role == Role::Source
                    && drive.volume.uuid == uuid
                    && parent == drive.metadata_dir(),
                "Migration may write only a source's own metadata, never backup/scratch indexes or source media"
            );
            break;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub uuid: String,
    pub relative: PathBuf,
}

/// The terminal is restored by the caller before invoking fzf.
pub fn search(source_uuid: Option<&str>) -> Result<Option<SearchHit>> {
    use std::os::unix::ffi::OsStringExt;
    use std::process::{Command, Stdio};
    let inputs: Vec<_> = source_uuid.map(PathBuf::from).into_iter().collect();
    let selections = select(&inputs)?;
    ensure!(!selections.is_empty(), "No source catalogs to search");
    let mut child = Command::new("fzf")
        .args([
            "--read0",
            "--print0",
            "--exact",
            "--no-multi",
            "--no-print-query",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Cannot start fzf; install fzf to search source paths")?;
    let input = child.stdin.take().context("fzf input unavailable")?;
    let output = std::thread::scope(|scope| -> Result<_> {
        let feed = scope.spawn(|| write(&selections, input));
        let output = child.wait_with_output()?;
        let result = feed
            .join()
            .map_err(|_| anyhow::anyhow!("Path export thread failed"))?;
        if !matches!(output.status.code(), Some(1 | 130)) {
            if let Err(error) = result {
                let broken_pipe = error.chain().any(|e| {
                    e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
                });
                if !broken_pipe {
                    return Err(error);
                }
            }
        }
        Ok(output)
    })?;
    if matches!(output.status.code(), Some(1 | 130)) {
        return Ok(None);
    }
    ensure!(output.status.success(), "fzf failed");
    let raw = output
        .stdout
        .strip_suffix(&[0])
        .context("fzf returned an incomplete path")?;
    ensure!(!raw.contains(&0), "fzf returned more than one selection");
    let path = PathBuf::from(std::ffi::OsString::from_vec(raw.to_vec()));
    let mut candidates = Vec::new();
    for selection in &selections {
        let Ok(relative) = path.strip_prefix(&selection.root) else {
            continue;
        };
        manifest::validate_path(relative)?;
        let file = File::open(&selection.catalog.path)?;
        let exists = if manifest::binary_file(&file)? {
            binary::Index::from_file(file)?
                .find_path(relative.as_os_str().as_bytes())?
                .is_some()
        } else {
            Manifest::load(&selection.catalog.path)?
                .entries
                .iter()
                .any(|e| e.path().is_ok_and(|p| p == relative))
        };
        if exists {
            candidates.push(SearchHit {
                uuid: selection.catalog.summary.header.volume.uuid.clone(),
                relative: relative.into(),
            });
        }
    }
    ensure!(
        candidates.len() == 1,
        "Selected path belongs to multiple recorded sources; search one drive by UUID"
    );
    Ok(candidates.pop())
}

pub fn reveal(hit: &SearchHit) -> Result<()> {
    manifest::validate_path(&hit.relative)?;
    let mounted = mounted_sources();
    let root = mounted
        .get(&hit.uuid)
        .context("Source is offline; reconnect its matching volume before revealing this file")?;
    let drive = Drive::open(root)?;
    ensure!(
        drive.volume.uuid == hit.uuid && drive.sentinel.role == Role::Source,
        "Source identity changed"
    );
    // Refuse symlinks and nested mounts before asking Finder to reveal a path.
    filesystem::Root::open(root)?
        .file(&hit.relative, false)?
        .open()?;
    let status = std::process::Command::new("open")
        .arg("-R")
        .arg(root.join(&hit.relative))
        .status()?;
    ensure!(status.success(), "Finder could not reveal the file");
    Ok(())
}
