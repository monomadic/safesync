use safesync::{
    binary, drive, filesystem,
    manifest::{self, Manifest, RecordedDrive},
    scan,
};
use std::{
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("safesync-binary-{}", manifest::generation()));
        fs::create_dir_all(root.join("source")).unwrap();
        Self {
            root: root.canonicalize().unwrap(),
        }
    }
    fn manifest(&self, hash: bool) -> Manifest {
        let mut m = scan::scan(
            &self.root.join("source"),
            filesystem::Volume {
                uuid: "binary-source".into(),
                name: "Source".into(),
                filesystem: "apfs".into(),
            },
            hash,
            |_| {},
        )
        .unwrap();
        m.header.drive = Some(RecordedDrive {
            role: drive::Role::Source,
            source_uuid: None,
        });
        m
    }
    fn put(&self, name: impl AsRef<Path>, data: &[u8]) {
        let path = self.root.join("source").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }
    fn save(&self, m: &Manifest) -> PathBuf {
        let path = self.root.join("index.ssi");
        m.save_new(&path).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn u64_at(b: &[u8], p: usize) -> usize {
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap()) as usize
}
fn redigest(b: &mut [u8]) {
    use sha2::{Digest, Sha256};
    let end = b.len() - 36;
    let digest = Sha256::digest(&b[..end]);
    b[end..end + 32].copy_from_slice(&digest);
}

#[test]
fn binary_roundtrip_sorted_paths_fingerprints_and_raw_export() {
    let f = Fixture::new();
    for name in ["z", "a/tab\tnewline\n", "raw"] {
        f.put(name, b"same content");
    }
    let mut m = f.manifest(true);
    let raw = m
        .entries
        .iter_mut()
        .find(|e| e.path().unwrap() == Path::new("raw"))
        .unwrap();
    raw.path_base64 = manifest::encode_path(&PathBuf::from(std::ffi::OsString::from_vec(vec![
        b'a', b'/', 255,
    ])));
    m.entries[0].sha256 = None;
    m.header.content_hashed = false;
    m.entries.reverse();
    let path = f.save(&m);
    let index = binary::Index::open(&path).unwrap();
    assert_eq!(index.summary().files, 3);
    assert_eq!(Manifest::summary(&path).unwrap().bytes, Some(36));
    let loaded = Manifest::load(&path).unwrap();
    assert_eq!(loaded.header.device, m.header.device);
    let mut expected = m.entries.clone();
    expected.sort_by_key(|e| e.path().unwrap());
    assert_eq!(loaded.entries, expected);
    let digest =
        binary::decode_hash(m.entries.iter().find_map(|e| e.sha256.as_deref()).unwrap()).unwrap();
    assert_eq!(index.find_fingerprint(&digest).unwrap().len(), 2);
    assert!(index.find_fingerprint(&[9; 32]).unwrap().is_empty());
    assert!(index.find_path(b"z").unwrap().is_some());
    assert!(index.find_path(b"absent").unwrap().is_none());
    let mut out = Vec::new();
    index
        .write_paths(Path::new("/new mount"), &mut out, 0)
        .unwrap();
    let mut expected_out = Vec::new();
    for entry in &expected {
        use std::os::unix::ffi::OsStrExt;
        expected_out.extend_from_slice(b"/new mount/");
        expected_out.extend_from_slice(entry.path().unwrap().as_os_str().as_bytes());
        expected_out.push(0);
    }
    assert_eq!(out, expected_out);
    assert!(
        m.save_new(&path).is_err(),
        "immutable publication never overwrites"
    );
    assert!(!fs::read_dir(&f.root).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".partial")
    }));
}

#[test]
fn empty_and_unhashed_indexes_omit_fingerprint_regions() {
    for empty in [true, false] {
        let f = Fixture::new();
        if !empty {
            f.put("empty", b"");
        }
        let m = f.manifest(false);
        let path = f.save(&m);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(u64_at(&bytes, 160), 0); // fingerprint region length
        assert_eq!(u64_at(&bytes, 176), 0); // fingerprint index length
        assert_eq!(Manifest::load(&path).unwrap().entries, m.entries);
    }
}

#[test]
fn summary_requires_complete_structurally_valid_binary_file() {
    let f = Fixture::new();
    f.put("clip", b"hello");
    let path = f.save(&f.manifest(true));
    let original = fs::read(&path).unwrap();
    for len in [0, 3, 255, original.len() - 36, original.len() - 1] {
        fs::write(&path, &original[..len]).unwrap();
        assert!(Manifest::summary(&path).is_err(), "len {len}");
    }
    let mut extra = original.clone();
    extra.push(0);
    fs::write(&path, extra).unwrap();
    assert!(Manifest::summary(&path).is_err());
    for (offset, data) in [
        (4, 2u64.to_le_bytes().to_vec()),
        (8, 512u64.to_le_bytes().to_vec()),
        (104, u64::MAX.to_le_bytes().to_vec()),
        (120, 256u64.to_le_bytes().to_vec()),
        (128, 55u64.to_le_bytes().to_vec()),
        (80, u64::MAX.to_le_bytes().to_vec()),
        (200, u64::MAX.to_le_bytes().to_vec()),
        (248, 2u32.to_le_bytes().to_vec()),
    ] {
        let mut bytes = original.clone();
        bytes[offset..offset + data.len()].copy_from_slice(&data);
        fs::write(&path, bytes).unwrap();
        assert!(Manifest::summary(&path).is_err(), "offset {offset}");
    }
}

#[test]
fn full_read_checks_digest_paths_records_and_fingerprint_index() {
    let f = Fixture::new();
    f.put("aaa", b"data");
    f.put("bbb", b"data");
    let path = f.save(&f.manifest(true));
    let original = fs::read(&path).unwrap();
    let records = u64_at(&original, 120);
    let paths = u64_at(&original, 136);
    let hashids = u64_at(&original, 168);
    let mut bytes = original.clone();
    bytes[records + 8] ^= 1;
    fs::write(&path, &bytes).unwrap();
    assert!(
        Manifest::summary(&path).is_ok(),
        "summaries deliberately defer the digest"
    );
    assert!(Manifest::load(&path).is_err());
    for (offset, data) in [
        (paths, b"../".to_vec()),
        (paths + 4, b"aaa".to_vec()),
        (paths + 3, vec![1]),
        (records + 40, u64::MAX.to_le_bytes().to_vec()),
        (records + 24, 1_000_000_000u32.to_le_bytes().to_vec()),
        (records + 52, 2u32.to_le_bytes().to_vec()),
        (hashids, 99u32.to_le_bytes().to_vec()),
    ] {
        let mut bytes = original.clone();
        bytes[offset..offset + data.len()].copy_from_slice(&data);
        redigest(&mut bytes);
        fs::write(&path, bytes).unwrap();
        assert!(Manifest::load(&path).is_err(), "offset {offset}");
    }
}

#[test]
fn writer_rejects_non_sources_bad_paths_and_cross_device_entries() {
    let f = Fixture::new();
    f.put("a", b"one");
    f.put("b", b"two");
    let m = f.manifest(false);
    for role in [drive::Role::Backup, drive::Role::Scratch] {
        let mut bad = m.clone();
        bad.header.drive.as_mut().unwrap().role = role;
        assert!(bad.save_new(&f.root.join("bad.ssi")).is_err());
    }
    for path in ["/absolute", "../escape", "a//b", "a\0b"] {
        let mut bad = m.clone();
        bad.entries[0].path_base64 = manifest::encode_path(Path::new(path));
        assert!(bad.save_new(&f.root.join("bad.ssi")).is_err());
    }
    let mut bad = m.clone();
    bad.entries[0].stamp.device += 1;
    assert!(bad.save_new(&f.root.join("bad.ssi")).is_err());
    let mut bad = m;
    bad.entries[0].stamp.mtime_nanos = -1;
    assert!(bad.save_new(&f.root.join("bad.ssi")).is_err());
}

#[test]
fn path_selection_deduplicates_generations_and_resolves_mounts_by_uuid() {
    use safesync::paths::{self, Catalog};
    use std::collections::BTreeMap;
    let f = Fixture::new();
    f.put("clip", b"bytes");
    let mut old = f.manifest(false);
    old.header.generation = "100-1-0".into();
    let old_path = f.root.join("old.jsonl");
    old.save_new(&old_path).unwrap();
    let mut current = old.clone();
    current.header.generation = "200-1-0".into();
    let current_path = f.save(&current);
    let copy = f.root.join("copy.ssi");
    current.export(&copy).unwrap();
    let catalogs = paths::newest(vec![
        Catalog::open(old_path).unwrap(),
        Catalog::open(current_path).unwrap(),
        Catalog::open(copy).unwrap(),
    ]);
    assert_eq!(catalogs.len(), 1);
    assert_eq!(catalogs[0].summary.header.generation, "200-1-0");
    let mut mounts =
        BTreeMap::from([("different-volume".into(), PathBuf::from("/Volumes/Source"))]);
    let offline = paths::resolve(catalogs.clone(), &mounts).unwrap();
    assert_eq!(offline[0].root, f.root.join("source"));
    mounts.insert("binary-source".into(), PathBuf::from("/Volumes/Source 2"));
    let online = paths::resolve(catalogs, &mounts).unwrap();
    let mut out = Vec::new();
    paths::write(&online, &mut out, true).unwrap();
    assert_eq!(out, b"/Volumes/Source 2/clip\0");
    let mut out = Vec::new();
    paths::write(&online, &mut out, false).unwrap();
    assert_eq!(out, b"/Volumes/Source 2/clip\n");
    // Same-name sources remain distinct by UUID.
    let mut other = current;
    other.header.volume.uuid = "other-source".into();
    let path = f.root.join("other.ssi");
    other.save_new(&path).unwrap();
    assert_eq!(
        paths::newest(vec![
            online[0].catalog.clone(),
            Catalog::open(path).unwrap()
        ])
        .len(),
        2
    );
}

#[test]
fn cli_exports_and_searches_only_newest_sources_and_converts_legacy_files() {
    use std::process::Command;
    let f = Fixture::new();
    f.put("clip\t\n.mov", b"bytes");
    let library = f
        .root
        .join("Library/Application Support/safesync/manifests");
    fs::create_dir_all(&library).unwrap();
    let mut old = f.manifest(true);
    old.header.generation = "100-1-0".into();
    let legacy = library.join("source-100.jsonl");
    old.export(&legacy).unwrap();
    let mut m = old.clone();
    m.header.generation = "200-1-0".into();
    m.export(&library.join("source-200.ssi")).unwrap();
    let mut backup = old.clone();
    backup.header.drive.as_mut().unwrap().role = drive::Role::Backup;
    backup.header.volume.uuid = "backup".into();
    backup.export(&library.join("backup.jsonl")).unwrap();
    let cli = |args: &[&std::ffi::OsStr]| {
        Command::new(env!("CARGO_BIN_EXE_safesync"))
            .args(args)
            .env("HOME", &f.root)
            .output()
            .unwrap()
    };
    let output = cli(&["paths".as_ref(), "--print0".as_ref()]);
    assert!(output.status.success(), "{:?}", output);
    use std::os::unix::ffi::OsStrExt;
    let mut expected = f
        .root
        .join("source/clip\t\n.mov")
        .as_os_str()
        .as_bytes()
        .to_vec();
    expected.push(0);
    assert_eq!(output.stdout, expected);
    let output = cli(&["paths".as_ref()]);
    assert!(output.status.success(), "{:?}", output);
    *expected.last_mut().unwrap() = b'\n';
    assert_eq!(output.stdout, expected);
    let output = cli(&[
        "lookup".as_ref(),
        "--file".as_ref(),
        f.root.join("source/clip\t\n.mov").as_os_str(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{:?}", output);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["manifests_searched"], 1);
    assert_eq!(report["matches"].as_array().unwrap().len(), 1);
    let original = fs::read(&legacy).unwrap();
    let convert = f.root.join("convert.jsonl");
    old.save_new(&convert).unwrap();
    let output = cli(&["migrate".as_ref(), convert.as_os_str()]);
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        Manifest::load(&convert.with_extension("ssi"))
            .unwrap()
            .entries,
        old.entries
    );
    assert_eq!(fs::read(&legacy).unwrap(), original);
    let output = cli(&["migrate".as_ref(), convert.as_os_str()]);
    assert!(output.status.success());
    let output = cli(&["paths".as_ref(), library.join("backup.jsonl").as_os_str()]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}
