//! SSIX v1: immutable, little-endian source indexes. See BINARY_FORMAT.md.
use crate::{
    drive::Role as DriveRole,
    filesystem::{Stamp, Volume},
    manifest::{self, Entry, Header, Manifest, RecordedDrive, Role, Summary},
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    path::Path,
    sync::OnceLock,
};

pub const MAGIC: &[u8; 4] = b"SSIX";
pub const HEADER_LEN: usize = 256;
pub const RECORD_LEN: usize = 56;
const TRAILER_LEN: u64 = 36;
const MAX_HEADER_HEAP: u64 = 16 * 1024 * 1024;
const VERSION: u32 = 1;

fn u32_at(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(b[p..p + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}
fn put32(b: &mut [u8], p: usize, n: u32) {
    b[p..p + 4].copy_from_slice(&n.to_le_bytes());
}
fn put64(b: &mut [u8], p: usize, n: u64) {
    b[p..p + 8].copy_from_slice(&n.to_le_bytes());
}
fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).context("Index offset overflow")
}
fn mul(a: u64, b: u64) -> Result<u64> {
    a.checked_mul(b).context("Index length overflow")
}
fn align(n: u64) -> Result<u64> {
    Ok(add(n, 7)? & !7)
}

#[derive(Clone, Copy, Default)]
struct Region {
    offset: u64,
    len: u64,
}
impl Region {
    fn end(self) -> Result<u64> {
        add(self.offset, self.len)
    }
    fn slice(self, bytes: &[u8]) -> &[u8] {
        &bytes[self.offset as usize..(self.offset + self.len) as usize]
    }
}

struct Layout {
    summary: Summary,
    device: u64,
    hashed: u64,
    regions: [Region; 6],
}

fn source_header(header: &Header) -> Result<()> {
    manifest::validate_header(header)?;
    ensure!(
        header
            .drive
            .as_ref()
            .is_some_and(|d| d.role == DriveRole::Source && d.source_uuid.is_none()),
        "Binary indexes belong only to source drives"
    );
    Ok(())
}

fn read_layout(file: &mut File) -> Result<Layout> {
    let file_len = file.metadata()?.len();
    ensure!(
        file_len <= isize::MAX as u64,
        "Index exceeds addressable size"
    );
    file.rewind()?;
    let mut fixed = [0; HEADER_LEN];
    file.read_exact(&mut fixed)
        .context("Truncated binary header")?;
    ensure!(&fixed[..4] == MAGIC, "Invalid binary index magic");
    ensure!(
        u32_at(&fixed, 4) == VERSION,
        "Unsupported binary index version"
    );
    ensure!(
        u32_at(&fixed, 8) as usize == HEADER_LEN,
        "Invalid binary header length"
    );
    let flags = u32_at(&fixed, 12);
    ensure!(flags & !3 == 0, "Unknown binary header flags");
    ensure!(
        u32_at(&fixed, 248) == 1,
        "Binary index is not a source catalog"
    );
    ensure!(
        u32_at(&fixed, 252) == manifest::SCHEMA,
        "Unsupported binary metadata schema"
    );
    let files = u64_at(&fixed, 80);
    let hashed = u64_at(&fixed, 96);
    ensure!(
        files <= u32::MAX as u64 && hashed <= files,
        "Invalid binary entry count"
    );
    ensure!(
        flags & 1 == 0 || hashed == files,
        "Incomplete fingerprint coverage"
    );
    let mut regions = [Region::default(); 6];
    let mut end = HEADER_LEN as u64;
    for (i, region) in regions.iter_mut().enumerate() {
        *region = Region {
            offset: u64_at(&fixed, 104 + i * 16),
            len: u64_at(&fixed, 112 + i * 16),
        };
        ensure!(
            region.offset == align(end)?,
            "Unaligned, overlapping or misplaced binary region"
        );
        end = region.end()?;
        ensure!(end <= file_len, "Binary region exceeds file bounds");
    }
    ensure!(
        end == file_len && regions[5].len == TRAILER_LEN,
        "Missing trailer or trailing binary data"
    );
    ensure!(
        regions[0].len <= MAX_HEADER_HEAP,
        "Oversized binary header heap"
    );
    ensure!(
        regions[1].len == mul(files, RECORD_LEN as u64)?,
        "Invalid record table length"
    );
    ensure!(
        regions[3].len == if hashed == 0 { 0 } else { mul(files, 32)? },
        "Invalid fingerprint table length"
    );
    ensure!(
        regions[4].len == mul(hashed, 4)?,
        "Invalid fingerprint index length"
    );
    ensure!(regions[2].len >= mul(files, 2)?, "Path heap is too short");
    ensure!(files != 0 || regions[2].len == 0, "Empty index has paths");
    let mut heap = vec![0; regions[0].len as usize];
    file.seek(SeekFrom::Start(regions[0].offset))?;
    file.read_exact(&mut heap)?;
    let mut fields = Vec::new();
    let mut position = 0u64;
    for i in 0..6 {
        let offset = u32_at(&fixed, 200 + i * 8) as u64;
        let len = u32_at(&fixed, 204 + i * 8) as u64;
        ensure!(offset == position, "Invalid header string offset");
        position = add(offset, len)?;
        ensure!(position <= heap.len() as u64, "Header string exceeds heap");
        fields.push(&heap[offset as usize..position as usize]);
    }
    ensure!(
        position == heap.len() as u64,
        "Unreferenced header string data"
    );
    let text = |i: usize| -> Result<String> {
        Ok(std::str::from_utf8(fields[i])
            .context("Invalid header UTF-8")?
            .into())
    };
    let mut exclusions = Vec::new();
    let mut rest = fields[5];
    while !rest.is_empty() {
        ensure!(rest.len() >= 4, "Truncated exclusion length");
        let len = u32_at(rest, 0) as usize;
        rest = &rest[4..];
        ensure!(len <= rest.len(), "Truncated exclusion string");
        exclusions.push(
            std::str::from_utf8(&rest[..len])
                .context("Invalid exclusion UTF-8")?
                .into(),
        );
        rest = &rest[len..];
    }
    let header = Header {
        schema: manifest::SCHEMA,
        generation: text(0)?,
        role: if flags & 2 == 0 {
            Role::Inventory
        } else {
            Role::OfflineSnapshot
        },
        volume: Volume {
            uuid: text(1)?,
            name: text(2)?,
            filesystem: text(3)?,
        },
        drive: Some(RecordedDrive {
            role: DriveRole::Source,
            source_uuid: None,
        }),
        root_base64: manifest::encode_path(Path::new(std::ffi::OsStr::from_bytes(fields[4]))),
        root_file_id: u64_at(&fixed, 24),
        device: Some(u64_at(&fixed, 16)),
        started_unix: u64_at(&fixed, 32),
        finished_unix: u64_at(&fixed, 40),
        hash_algorithm: "sha256".into(),
        content_hashed: flags & 1 != 0,
        exclusions,
        reused_hashes: u64_at(&fixed, 48),
        skipped_symlinks: u64_at(&fixed, 56),
        skipped_special: u64_at(&fixed, 64),
        skipped_mounts: u64_at(&fixed, 72),
    };
    source_header(&header)?;
    let mut trailer = [0; TRAILER_LEN as usize];
    file.seek(SeekFrom::Start(regions[5].offset))?;
    file.read_exact(&mut trailer)
        .context("Truncated binary trailer")?;
    ensure!(&trailer[32..] == MAGIC, "Missing committed binary trailer");
    Ok(Layout {
        summary: Summary {
            header,
            files: files as usize,
            bytes: Some(u64_at(&fixed, 88)),
        },
        device: u64_at(&fixed, 16),
        hashed,
        regions,
    })
}

pub fn summary(mut file: File) -> Result<Summary> {
    Ok(read_layout(&mut file)?.summary)
}

/// Read-only mapping of a committed immutable index. Never map in-place writers.
struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}
impl Mapping {
    fn new(file: &File) -> Result<Self> {
        let len = usize::try_from(file.metadata()?.len())?;
        ensure!(
            len > 0 && len <= isize::MAX as usize,
            "Invalid mapping length"
        );
        // SAFETY: the file is opened read-only, len was checked, and the mapping
        // is owned until Drop. Published indexes are immutable and replaced by
        // new generations, never truncated in place.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        ensure!(
            ptr != libc::MAP_FAILED,
            "Cannot map index: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self { ptr, len })
    }
    fn bytes(&self) -> &[u8] {
        // SAFETY: this mapping remains live for the borrowed slice's lifetime.
        unsafe { std::slice::from_raw_parts(self.ptr.cast(), self.len) }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

pub struct Index {
    mapping: Mapping,
    layout: Layout,
    checked: OnceLock<Result<(), String>>,
}

pub struct Record<'a> {
    pub path: &'a [u8],
    pub stamp: Stamp,
    pub fingerprint: Option<&'a [u8; 32]>,
}
impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_file(File::open(path)?)
    }
    pub fn from_file(mut file: File) -> Result<Self> {
        let layout = read_layout(&mut file)?;
        let mapping = Mapping::new(&file)?;
        ensure!(
            mapping.len as u64 == layout.regions[5].end()?,
            "Index changed while opening"
        );
        Ok(Self {
            mapping,
            layout,
            checked: OnceLock::new(),
        })
    }
    pub fn summary(&self) -> &Summary {
        &self.layout.summary
    }
    fn raw_record(&self, i: usize) -> &[u8] {
        let start = self.layout.regions[1].offset as usize + i * RECORD_LEN;
        &self.mapping.bytes()[start..start + RECORD_LEN]
    }
    fn raw_hash(&self, i: usize) -> &[u8; 32] {
        let start = self.layout.regions[3].offset as usize + i * 32;
        self.mapping.bytes()[start..start + 32].try_into().unwrap()
    }
    fn validate_all(&self) -> Result<()> {
        let bytes = self.mapping.bytes();
        let trailer = self.layout.regions[5].offset as usize;
        ensure!(
            Sha256::digest(&bytes[..trailer]).as_slice() == &bytes[trailer..trailer + 32],
            "Binary index checksum mismatch"
        );
        let mut previous_path: Option<&[u8]> = None;
        let heap = self.layout.regions[2].slice(bytes);
        let mut path_end = 0u64;
        let mut total = 0u64;
        let mut hashed = 0u64;
        for i in 0..self.summary().files {
            let row = self.raw_record(i);
            let flags = u32_at(row, 52);
            ensure!(flags & !1 == 0, "Unknown record flags");
            ensure!(
                u32_at(row, 24) < 1_000_000_000 && u32_at(row, 36) < 1_000_000_000,
                "Invalid timestamp nanoseconds"
            );
            ensure!(
                u64_at(row, 40) == path_end,
                "Invalid path offset or ordering"
            );
            let len = u32_at(row, 48) as u64;
            let end = add(path_end, len)?;
            path_end = add(end, 1)?;
            ensure!(path_end <= heap.len() as u64, "Path exceeds heap");
            let path = &heap[(end - len) as usize..end as usize];
            ensure!(heap[end as usize] == 0, "Path lacks NUL terminator");
            manifest::validate_path(Path::new(std::ffi::OsStr::from_bytes(path)))?;
            ensure!(
                previous_path.is_none_or(|p| p < path),
                "Unsorted or duplicate binary path"
            );
            previous_path = Some(path);
            total = add(total, u64_at(row, 8))?;
            if flags & 1 != 0 {
                hashed += 1;
                ensure!(self.layout.hashed > 0, "Missing fingerprints");
            } else if self.layout.hashed > 0 {
                ensure!(
                    self.raw_hash(i).iter().all(|b| *b == 0),
                    "Unhashed record contains fingerprint bytes"
                );
            }
        }
        ensure!(path_end == heap.len() as u64, "Unreferenced path data");
        ensure!(
            total == self.summary().bytes.unwrap() && hashed == self.layout.hashed,
            "Binary totals mismatch"
        );
        let mut previous = None;
        for chunk in self.layout.regions[4].slice(bytes).chunks_exact(4) {
            let index = u32_at(chunk, 0) as usize;
            ensure!(
                index < self.summary().files && u32_at(self.raw_record(index), 52) == 1,
                "Invalid fingerprint index record"
            );
            let key = (self.raw_hash(index), index);
            ensure!(
                previous.is_none_or(|p| p < key),
                "Unsorted or duplicate fingerprint index"
            );
            previous = Some(key);
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<()> {
        match self
            .checked
            .get_or_init(|| self.validate_all().map_err(|e| format!("{e:#}")))
        {
            Ok(()) => Ok(()),
            Err(e) => anyhow::bail!("{e}"),
        }
    }
    fn record_unchecked(&self, i: usize) -> Record<'_> {
        let row = self.raw_record(i);
        let offset = u64_at(row, 40) as usize;
        let len = u32_at(row, 48) as usize;
        Record {
            path: &self.layout.regions[2].slice(self.mapping.bytes())[offset..offset + len],
            stamp: Stamp {
                device: self.layout.device,
                file_id: u64_at(row, 0),
                size: u64_at(row, 8),
                mtime_seconds: u64_at(row, 16) as i64,
                mtime_nanos: u32_at(row, 24) as i64,
                ctime_seconds: u64_at(row, 28) as i64,
                ctime_nanos: u32_at(row, 36) as i64,
            },
            fingerprint: (u32_at(row, 52) == 1).then(|| self.raw_hash(i)),
        }
    }
    pub fn records(&self) -> Result<impl Iterator<Item = Record<'_>>> {
        self.validate()?;
        Ok((0..self.summary().files).map(|i| self.record_unchecked(i)))
    }
    pub fn find_path(&self, path: &[u8]) -> Result<Option<Record<'_>>> {
        self.validate()?;
        let (mut lo, mut hi) = (0, self.summary().files);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.record_unchecked(mid).path < path {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok((lo < self.summary().files)
            .then(|| self.record_unchecked(lo))
            .filter(|r| r.path == path))
    }
    pub fn find_fingerprint(&self, hash: &[u8; 32]) -> Result<Vec<Record<'_>>> {
        self.validate()?;
        let ids = self.layout.regions[4].slice(self.mapping.bytes());
        let get = |n| u32_at(ids, n * 4) as usize;
        let (mut lo, mut hi) = (0, self.layout.hashed as usize);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.raw_hash(get(mid)) < hash {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut matches = Vec::new();
        while lo < self.layout.hashed as usize && self.raw_hash(get(lo)) == hash {
            matches.push(self.record_unchecked(get(lo)));
            lo += 1;
        }
        Ok(matches)
    }
    pub fn to_manifest(&self) -> Result<Manifest> {
        let entries = self
            .records()?
            .map(|r| Entry {
                path_base64: manifest::encode_path(Path::new(std::ffi::OsStr::from_bytes(r.path))),
                stamp: r.stamp,
                sha256: r.fingerprint.map(|h| crate::filesystem::hex(h)),
            })
            .collect();
        Ok(Manifest {
            header: self.summary().header.clone(),
            entries,
        })
    }
    pub fn write_paths(&self, root: &Path, mut output: impl Write, terminator: u8) -> Result<()> {
        ensure!(
            root.is_absolute() && !root.as_os_str().as_bytes().contains(&0),
            "Invalid export root"
        );
        let root = root.as_os_str().as_bytes();
        for record in self.records()? {
            output.write_all(root)?;
            if !root.ends_with(b"/") {
                output.write_all(b"/")?;
            }
            output.write_all(record.path)?;
            output.write_all(&[terminator])?;
        }
        Ok(())
    }
}

pub fn decode_hash(hash: &str) -> Result<[u8; 32]> {
    ensure!(hash.len() == 64, "Invalid fingerprint length");
    let mut out = [0; 32];
    for (i, bytes) in hash.as_bytes().chunks_exact(2).enumerate() {
        let digit = |b: u8| -> Result<u8> {
            match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                _ => anyhow::bail!("Invalid fingerprint"),
            }
        };
        out[i] = digit(bytes[0])? * 16 + digit(bytes[1])?;
    }
    Ok(out)
}

pub fn write(manifest: &Manifest, mut output: impl Write) -> Result<()> {
    source_header(&manifest.header)?;
    manifest.validate()?;
    let h = &manifest.header;
    ensure!(
        manifest.entries.len() <= u32::MAX as usize,
        "Too many binary records"
    );
    let device = h
        .device
        .or_else(|| manifest.entries.first().map(|e| e.stamp.device))
        .unwrap_or(0);
    let mut rows = Vec::with_capacity(manifest.entries.len());
    for e in &manifest.entries {
        ensure!(e.stamp.device == device, "Index crosses devices");
        ensure!(
            (0..1_000_000_000).contains(&e.stamp.mtime_nanos)
                && (0..1_000_000_000).contains(&e.stamp.ctime_nanos),
            "Invalid timestamp nanoseconds"
        );
        let path = e.path()?.as_os_str().as_bytes().to_vec();
        ensure!(path.len() <= u32::MAX as usize, "Path too long");
        rows.push((path, e, e.sha256.as_deref().map(decode_hash).transpose()?));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hash_ids: Vec<u32> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.2.is_some())
        .map(|(i, _)| i as u32)
        .collect();
    hash_ids.sort_by_key(|i| (rows[*i as usize].2.unwrap(), *i));
    let root = manifest::decode_path(&h.root_base64)?;
    let mut exclusions = Vec::new();
    for s in &h.exclusions {
        exclusions.extend_from_slice(&u32::try_from(s.len())?.to_le_bytes());
        exclusions.extend_from_slice(s.as_bytes());
    }
    let fields = [
        h.generation.as_bytes(),
        h.volume.uuid.as_bytes(),
        h.volume.name.as_bytes(),
        h.volume.filesystem.as_bytes(),
        root.as_os_str().as_bytes(),
        &exclusions,
    ];
    let mut fixed = [0; HEADER_LEN];
    fixed[..4].copy_from_slice(MAGIC);
    put32(&mut fixed, 4, VERSION);
    put32(&mut fixed, 8, HEADER_LEN as u32);
    put32(
        &mut fixed,
        12,
        u32::from(h.content_hashed)
            | if matches!(h.role, Role::OfflineSnapshot) {
                2
            } else {
                0
            },
    );
    for (p, n) in [
        (16, device),
        (24, h.root_file_id),
        (32, h.started_unix),
        (40, h.finished_unix),
        (48, h.reused_hashes),
        (56, h.skipped_symlinks),
        (64, h.skipped_special),
        (72, h.skipped_mounts),
        (80, rows.len() as u64),
        (96, hash_ids.len() as u64),
    ] {
        put64(&mut fixed, p, n);
    }
    put32(&mut fixed, 248, 1);
    put32(&mut fixed, 252, manifest::SCHEMA);
    let mut heap = Vec::new();
    for (i, s) in fields.into_iter().enumerate() {
        put32(&mut fixed, 200 + i * 8, u32::try_from(heap.len())?);
        put32(&mut fixed, 204 + i * 8, u32::try_from(s.len())?);
        heap.extend_from_slice(s);
    }
    ensure!(
        heap.len() as u64 <= MAX_HEADER_HEAP,
        "Oversized binary header heap"
    );
    let mut path_len = 0;
    let mut total = 0;
    for r in &rows {
        path_len = add(path_len, add(r.0.len() as u64, 1)?)?;
        total = add(total, r.1.stamp.size)?;
    }
    put64(&mut fixed, 88, total);
    let lengths = [
        heap.len() as u64,
        mul(rows.len() as u64, 56)?,
        path_len,
        if hash_ids.is_empty() {
            0
        } else {
            mul(rows.len() as u64, 32)?
        },
        mul(hash_ids.len() as u64, 4)?,
        TRAILER_LEN,
    ];
    let mut regions = [Region::default(); 6];
    let mut end = HEADER_LEN as u64;
    for (i, len) in lengths.into_iter().enumerate() {
        regions[i] = Region {
            offset: align(end)?,
            len,
        };
        end = regions[i].end()?;
        put64(&mut fixed, 104 + i * 16, regions[i].offset);
        put64(&mut fixed, 112 + i * 16, len);
    }
    let mut digest = Sha256::new();
    let mut written = 0u64;
    {
        let mut emit = |b: &[u8]| -> Result<()> {
            output.write_all(b)?;
            digest.update(b);
            written += b.len() as u64;
            Ok(())
        };
        emit(&fixed)?;
        emit(&heap)?;
    }
    let pad = |region: Region,
               output: &mut dyn Write,
               digest: &mut Sha256,
               written: &mut u64|
     -> Result<()> {
        let n = region
            .offset
            .checked_sub(*written)
            .context("Invalid writer offset")?;
        ensure!(n < 8, "Invalid alignment padding");
        let zeros = [0; 8];
        output.write_all(&zeros[..n as usize])?;
        digest.update(&zeros[..n as usize]);
        *written += n;
        Ok(())
    };
    pad(regions[1], &mut output, &mut digest, &mut written)?;
    let mut path_offset = 0;
    for (path, e, hash) in &rows {
        let mut row = [0; RECORD_LEN];
        for (p, n) in [
            (0, e.stamp.file_id),
            (8, e.stamp.size),
            (16, e.stamp.mtime_seconds as u64),
            (28, e.stamp.ctime_seconds as u64),
            (40, path_offset),
        ] {
            put64(&mut row, p, n);
        }
        put32(&mut row, 24, e.stamp.mtime_nanos as u32);
        put32(&mut row, 36, e.stamp.ctime_nanos as u32);
        put32(&mut row, 48, path.len() as u32);
        put32(&mut row, 52, u32::from(hash.is_some()));
        output.write_all(&row)?;
        digest.update(row);
        written += RECORD_LEN as u64;
        path_offset += path.len() as u64 + 1;
    }
    pad(regions[2], &mut output, &mut digest, &mut written)?;
    for (path, _, _) in &rows {
        output.write_all(path)?;
        output.write_all(&[0])?;
        digest.update(path);
        digest.update([0]);
        written += path.len() as u64 + 1;
    }
    pad(regions[3], &mut output, &mut digest, &mut written)?;
    if !hash_ids.is_empty() {
        for (_, _, hash) in &rows {
            let hash = hash.unwrap_or([0; 32]);
            output.write_all(&hash)?;
            digest.update(hash);
            written += 32;
        }
    }
    pad(regions[4], &mut output, &mut digest, &mut written)?;
    for i in hash_ids {
        output.write_all(&i.to_le_bytes())?;
        digest.update(i.to_le_bytes());
        written += 4;
    }
    pad(regions[5], &mut output, &mut digest, &mut written)?;
    output.write_all(&digest.finalize())?;
    output.write_all(MAGIC)?;
    output.flush()?;
    Ok(())
}
