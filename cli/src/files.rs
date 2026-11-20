//! What `health` and `info` read from the store's files, through the formats' read-only decoders (Q-1051): the
//! log's segments, leases, derived files, the status file, the descriptor and the newest registry file.

use std::collections::BTreeMap;
use std::path::Path;

use kvik::Fs;
use kvik_log::directory::series::SeriesDirectory;
use kvik_log::directory::status::{StatusFile, counter, health, reason};
use kvik_log::directory::{self, Family, Frame};
use kvik_log::entries::Entry;
use kvik_log::lease::{LeaseSlot, read_leases};
use kvik_log::store::StoreDescriptor;

/// Every file under the store directory with its length, by path relative to it. A directory is told from a file by
/// having entries.
pub fn walk(fs: &dyn Fs, root: &Path) -> Vec<(String, u64)> {
    fn go(fs: &dyn Fs, root: &Path, rel: &str, out: &mut Vec<(String, u64)>, depth: u32) {
        let Ok(entries) = fs.read_dir(&root.join(rel)) else {
            return;
        };
        for e in entries {
            let path = if rel.is_empty() {
                e.name.clone()
            } else {
                format!("{rel}/{}", e.name)
            };
            let below = depth < 4 && fs.read_dir(&root.join(&path)).is_ok_and(|v| !v.is_empty());
            if below {
                go(fs, root, &path, out, depth + 1);
            } else {
                out.push((path, e.len));
            }
        }
    }
    let mut out = Vec::new();
    go(fs, root, "", &mut out, 0);
    out.sort();
    out
}

/// The kind of a store file by its place and name: its directory and extension.
pub fn kind_of(path: &str) -> String {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    match (dir, ext) {
        ("", _) => name.to_owned(),
        (d, "") => format!("{d}/"),
        (d, e) => format!("{d}/*.{e}"),
    }
}

/// Counts and bytes by [`kind_of`].
pub fn by_kind(files: &[(String, u64)]) -> BTreeMap<String, (u64, u64)> {
    let mut out: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for (p, len) in files {
        let e = out.entry(kind_of(p)).or_default();
        e.0 += 1;
        e.1 = e.1.saturating_add(*len);
    }
    out
}

/// The store descriptor.
pub fn descriptor(fs: &dyn Fs, dir: &Path) -> Option<StoreDescriptor> {
    kvik_log::store::read_descriptor(fs, dir).ok()
}

/// The lease files: reader id and slot, `None` for an invalid one.
pub fn leases(fs: &dyn Fs, dir: &Path, store_id: &[u8; 16]) -> Vec<([u8; 16], Option<LeaseSlot>)> {
    let mut v = read_leases(fs, &dir.join("leases"), store_id).unwrap_or_default();
    v.sort_by_key(|(id, _)| *id);
    v
}

/// The newest valid series directory.
pub fn series_directory(fs: &dyn Fs, dir: &Path, store_id: &[u8; 16]) -> Option<(Frame, SeriesDirectory)> {
    directory::read_latest(fs, dir, Family::Series, |b| SeriesDirectory::decode(b, store_id))
        .ok()
        .flatten()
}

/// The newest valid status file.
pub fn status(fs: &dyn Fs, dir: &Path, store_id: &[u8; 16]) -> Option<(Frame, StatusFile)> {
    directory::read_latest(fs, dir, Family::Status, |b| StatusFile::decode(b, store_id))
        .ok()
        .flatten()
}

/// The site zone and tzdata version in force: the newest registry file's last entries of each, else the
/// descriptor's.
pub fn in_force(fs: &dyn Fs, dir: &Path, d: &StoreDescriptor) -> (String, String) {
    let mut zone = d.site_zone.clone();
    let mut tzdata = d.tzdata.clone();
    if let Ok(Some((_, f))) = kvik_log::registry::read_latest(fs, dir, &d.store_id) {
        for (_, _, e) in &f.entries {
            match e {
                Entry::SiteZone { zone: z, .. } => zone = z.clone(),
                Entry::Tzdata { version } => tzdata = version.clone(),
                _ => {}
            }
        }
    }
    (zone, tzdata)
}

/// The name of a status counter.
pub fn counter_name(id: u16) -> String {
    match id {
        counter::QUEUE_DEPTH => "queue_depth".into(),
        counter::WAITED_NS => "waited_ns".into(),
        counter::BATCHES => "batches".into(),
        counter::SYNCS => "syncs".into(),
        counter::COMMIT_P50_NS => "commit_p50_ns".into(),
        counter::COMMIT_P99_NS => "commit_p99_ns".into(),
        counter::COMMIT_MAX_NS => "commit_max_ns".into(),
        counter::SEALED_SEGMENTS => "sealed_segments".into(),
        counter::DISK_FREE => "disk_free".into(),
        counter::DERIVED_FAILED => "derived_failed".into(),
        counter::UNLISTED => "unlisted".into(),
        other => format!("counter_{other}"),
    }
}

/// The name of a status reason.
pub fn reason_name(id: u16) -> String {
    match id {
        reason::DATA_LOST => "data_lost".into(),
        reason::INGEST_WAITING => "ingest_waiting".into(),
        reason::DISK_LOW => "disk_low".into(),
        reason::DISK_CRITICAL => "disk_critical".into(),
        reason::SEAL_BLOCKED => "seal_blocked".into(),
        reason::MANIFEST_REBUILT => "manifest_rebuilt".into(),
        other => format!("reason_{other}"),
    }
}

/// The name of a status health level.
pub fn health_name(level: u8) -> String {
    match level {
        health::OK => "ok".into(),
        health::DEGRADED => "degraded".into(),
        health::CRITICAL => "critical".into(),
        other => format!("level_{other}"),
    }
}

/// Lower-case hex.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_files_kind_is_its_directory_and_extension() {
        assert_eq!(kind_of("log/0000000000000001.kvl"), "log/*.kvl");
        assert_eq!(kind_of("MARK"), "MARK");
        assert_eq!(kind_of("leases/abc"), "leases/");
        let k = by_kind(&[("a/x.y".into(), 3), ("a/z.y".into(), u64::MAX)]);
        assert_eq!(k["a/*.y"], (2, u64::MAX));
    }
}
