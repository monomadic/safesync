use crate::{filesystem, manifest::Manifest};
use anyhow::Result;
use serde::Serialize;
use std::{ffi::OsStr, path::Path};

#[derive(Serialize)]
pub struct Match {
    pub volume_name: String,
    pub volume_uuid: String,
    pub generation: String,
    pub scanned_unix: u64,
    pub root_base64: String,
    pub path_base64: String,
    pub path_display: String,
    pub evidence: String,
}
#[derive(Serialize)]
pub struct Report {
    pub scope: &'static str,
    pub matches: Vec<Match>,
    pub unverified_candidates: usize,
    pub manifests_searched: usize,
    pub verdict: &'static str,
}
impl Report {
    pub fn exit_code(&self) -> i32 {
        if !self.matches.is_empty() {
            0
        } else if self.unverified_candidates > 0 {
            3
        } else {
            1
        }
    }
}
pub enum Query<'a> {
    Name(&'a OsStr),
    File(&'a Path),
}
pub fn lookup(manifests: &[Manifest], query: Query<'_>) -> Result<Report> {
    let fingerprint = match query {
        Query::File(path) => Some(filesystem::hash_path(path)?),
        _ => None,
    };
    let mut matches = Vec::new();
    let mut unknown = 0;
    for manifest in manifests {
        for entry in &manifest.entries {
            let path = entry.path()?;
            let evidence = match &query {
                Query::Name(name) => {
                    if path.file_name() != Some(*name) {
                        continue;
                    }
                    "exact_filename_only"
                }
                Query::File(_) => {
                    let (stamp, hash) = fingerprint.as_ref().expect("file query has fingerprint");
                    if entry.stamp.size != stamp.size {
                        continue;
                    }
                    match &entry.sha256 {
                        None => {
                            unknown += 1;
                            continue;
                        }
                        Some(saved) if saved != hash => continue,
                        Some(_) => "sha256_content_at_scan_time",
                    }
                }
            };
            matches.push(Match {
                volume_name: manifest.header.volume.name.clone(),
                volume_uuid: manifest.header.volume.uuid.clone(),
                generation: manifest.header.generation.clone(),
                scanned_unix: manifest.header.finished_unix,
                root_base64: manifest.header.root_base64.clone(),
                path_base64: entry.path_base64.clone(),
                path_display: path.to_string_lossy().into(),
                evidence: evidence.into(),
            });
        }
    }
    let verdict = if !matches.is_empty() {
        "recorded_match"
    } else if unknown > 0 {
        "unknown_missing_content_hashes"
    } else {
        "not_recorded_in_selected_manifests"
    };
    Ok(Report {
        scope: "historical_offline_inventory_not_live_presence",
        matches,
        unverified_candidates: unknown,
        manifests_searched: manifests.len(),
        verdict,
    })
}

/// Searches binary catalogs through their mappings; only matching paths become
/// owned strings. JSONL remains supported during migration.
pub fn lookup_paths(paths: &[std::path::PathBuf], query: Query<'_>) -> Result<Report> {
    use crate::{
        binary,
        manifest::{self, Header},
    };
    use std::{fs::File, os::unix::ffi::OsStrExt};
    let fingerprint = match &query {
        Query::File(path) => Some(filesystem::hash_path(path)?),
        _ => None,
    };
    let mut matches = Vec::new();
    let mut unknown = 0;
    let mut consider = |header: &Header, path: &Path, size: u64, hash: Option<&str>| {
        let evidence = match &query {
            Query::Name(name) => {
                if path.file_name() != Some(*name) {
                    return;
                }
                "exact_filename_only"
            }
            Query::File(_) => {
                let (stamp, expected) = fingerprint.as_ref().unwrap();
                if size != stamp.size {
                    return;
                }
                match hash {
                    None => {
                        unknown += 1;
                        return;
                    }
                    Some(h) if h != expected => return,
                    Some(_) => "sha256_content_at_scan_time",
                }
            }
        };
        matches.push(Match {
            volume_name: header.volume.name.clone(),
            volume_uuid: header.volume.uuid.clone(),
            generation: header.generation.clone(),
            scanned_unix: header.finished_unix,
            root_base64: header.root_base64.clone(),
            path_base64: manifest::encode_path(path),
            path_display: path.to_string_lossy().into(),
            evidence: evidence.into(),
        });
    };
    for path in paths {
        let file = File::open(path)?;
        if manifest::binary_file(&file)? {
            let index = binary::Index::from_file(file)?;
            let header = &index.summary().header;
            if let Some((stamp, hash)) = &fingerprint {
                for record in index.find_fingerprint(&binary::decode_hash(hash)?)? {
                    consider(
                        header,
                        Path::new(OsStr::from_bytes(record.path)),
                        record.stamp.size,
                        Some(hash),
                    );
                }
                for record in index
                    .records()?
                    .filter(|r| r.fingerprint.is_none() && r.stamp.size == stamp.size)
                {
                    consider(
                        header,
                        Path::new(OsStr::from_bytes(record.path)),
                        record.stamp.size,
                        None,
                    );
                }
            } else {
                for record in index.records()? {
                    consider(
                        header,
                        Path::new(OsStr::from_bytes(record.path)),
                        record.stamp.size,
                        None,
                    );
                }
            }
        } else {
            let m = Manifest::load(path)?;
            for entry in &m.entries {
                consider(
                    &m.header,
                    &entry.path()?,
                    entry.stamp.size,
                    entry.sha256.as_deref(),
                );
            }
        }
    }
    let verdict = if !matches.is_empty() {
        "recorded_match"
    } else if unknown > 0 {
        "unknown_missing_content_hashes"
    } else {
        "not_recorded_in_selected_manifests"
    };
    Ok(Report {
        scope: "historical_offline_inventory_not_live_presence",
        matches,
        unverified_candidates: unknown,
        manifests_searched: paths.len(),
        verdict,
    })
}
