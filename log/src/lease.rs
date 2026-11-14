//! Reader leases (`format.md` A.15, `spec.md` §7 Leases): the 128-byte lease file a reader process writes and
//! renews, and the writer's view of the lease files it finds.
//!
//! A reader names in its lease the manifest epoch it took; the writer keeps what a live lease may still read. Each
//! lease carries a refresh counter that the reader increments at every renewal. The writer expires a lease whose
//! counter has not changed for [`LEASE_EXPIRY`] of the writer's monotonic clock since the writer first observed
//! that value, and treats a lease of another boot as void; it deletes void and expired leases. A lease is never
//! synced.
//!
//! Nothing the writer removes today needs a lease: it deletes only segments that hold no batch (`spec.md` §3).
//! [`LeaseTable::oldest_live_epoch`] is what the removal of retired files (roadmap 6.3) and tier eviction (8.1)
//! must honour; new manifest epochs already continue above every epoch a lease file names (§7 Manifest).
//!
//! ```
//! use kvik_log::lease::{LeaseSlot, lease_name, parse_lease_name};
//!
//! let slot = LeaseSlot { epoch: 3, refresh_counter: 1, boot_id: [9; 16], created_at: 10 };
//! let file = slot.encode_file([1; 16], 0);
//! assert_eq!(LeaseSlot::decode_file(&file, &[1; 16]), Some(slot));
//! // A lease of another store is invalid.
//! assert_eq!(LeaseSlot::decode_file(&file, &[2; 16]), None);
//! let name = lease_name(&[0xab; 16]);
//! assert_eq!(parse_lease_name(&name), Some([0xab; 16]));
//! ```

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::time::Duration;

use crate::bytes::{i64_at, put, u32_at, u64_at};
use crate::crc::crc32c;
use crate::fs::{Fs, OpenMode};
use crate::header::{FileHeader, HEADER_LEN, kind};

/// A lease file's length.
pub const LEASE_LEN: usize = 128;
/// The lease slot's offset: just past the file header.
pub const SLOT_OFFSET: usize = HEADER_LEN;
/// How long the writer keeps a lease whose counter has not changed (`spec.md` §7 Leases).
pub const LEASE_EXPIRY: Duration = Duration::from_secs(3_600);
/// The longest a reader may go between renewals: half of [`LEASE_EXPIRY`] (`spec.md` §7 Readers).
pub const LEASE_REFRESH: Duration = Duration::from_secs(1_800);
/// How often the writer looks at the lease files, at most. A later look only makes an expiry later, never
/// earlier: a value is first observed when it is seen.
pub const LEASE_SCAN_INTERVAL: Duration = Duration::from_secs(60);
const EXT: &str = ".lease";

/// The slot of a lease file (A.15).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LeaseSlot {
    /// The manifest epoch the reader took.
    pub epoch: u64,
    /// Incremented by the reader at every renewal.
    pub refresh_counter: u64,
    /// The reader's boot identity, from the same source as the mark's; zero where the platform has none.
    pub boot_id: [u8; 16],
    /// When the reader created the lease, nanoseconds since the Unix epoch.
    pub created_at: i64,
}

impl LeaseSlot {
    /// The 64 bytes of the slot, `crc32c` included: what a renewal writes at [`SLOT_OFFSET`].
    pub fn encode(&self) -> [u8; LEASE_LEN - HEADER_LEN] {
        let mut b = [0u8; LEASE_LEN];
        self.put_slot(&mut b);
        let mut out = [0u8; LEASE_LEN - HEADER_LEN];
        out.copy_from_slice(&b[HEADER_LEN..]);
        out
    }

    fn put_slot(&self, b: &mut [u8; LEASE_LEN]) {
        put(b, 64, &self.epoch.to_le_bytes());
        put(b, 72, &self.refresh_counter.to_le_bytes());
        put(b, 80, &self.boot_id);
        put(b, 96, &self.created_at.to_le_bytes());
        let crc = crc32c(&b[64..124]);
        put(b, 124, &crc.to_le_bytes());
    }

    /// The whole file: an A.1 header (kind 8, `file_id` 0) then the slot.
    pub fn encode_file(&self, store_id: [u8; 16], generation: u32) -> [u8; LEASE_LEN] {
        let mut b = [0u8; LEASE_LEN];
        let h = FileHeader::new(kind::LEASE, generation, 0, self.created_at, store_id);
        b[..HEADER_LEN].copy_from_slice(&h.encode());
        self.put_slot(&mut b);
        b
    }

    /// The slot of a lease file of the store `store_id`; `None` if the header or the slot is invalid, of another
    /// kind or store, or of a newer major version. Bytes after the 128 are ignored.
    pub fn decode_file(bytes: &[u8], store_id: &[u8; 16]) -> Option<Self> {
        let b = bytes.get(..LEASE_LEN)?;
        let h = FileHeader::decode(b)?;
        if h.kind != kind::LEASE || &h.store_id != store_id || h.format_major > crate::FORMAT_MAJOR
        {
            return None;
        }
        if u32_at(b, 124) != crc32c(&b[64..124]) {
            return None;
        }
        let mut boot_id = [0u8; 16];
        boot_id.copy_from_slice(&b[80..96]);
        Some(Self {
            epoch: u64_at(b, 64),
            refresh_counter: u64_at(b, 72),
            boot_id,
            created_at: i64_at(b, 96),
        })
    }
}

/// The file name of a reader's lease: `<reader_id:32 hex>.lease` (A.12).
pub fn lease_name(reader_id: &[u8; 16]) -> String {
    let mut s = String::with_capacity(38);
    for b in reader_id {
        s.push_str(&format!("{b:02x}"));
    }
    s.push_str(EXT);
    s
}

/// The reader id a lease file name holds; `None` for any other name (readers and the writer match names exactly).
pub fn parse_lease_name(name: &str) -> Option<[u8; 16]> {
    let hex = name.strip_suffix(EXT)?;
    if hex.len() != 32
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(id)
}

fn read_lease(fs: &dyn Fs, path: &Path) -> io::Result<Vec<u8>> {
    let f = fs.open(path, OpenMode::Read)?;
    let mut b = vec![0u8; LEASE_LEN];
    let mut at = 0;
    while at < LEASE_LEN {
        let n = f.read_at(&mut b[at..], at as u64)?;
        if n == 0 {
            break;
        }
        at += n;
    }
    b.truncate(at);
    Ok(b)
}

/// Every lease file in `dir`: its reader id and its slot, `None` when invalid or unreadable. A missing directory
/// holds none; a file removed while listing is left out.
pub fn read_leases(
    fs: &dyn Fs,
    dir: &Path,
    store_id: &[u8; 16],
) -> io::Result<Vec<([u8; 16], Option<LeaseSlot>)>> {
    let entries = match fs.read_dir(dir) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for e in entries {
        let Some(id) = parse_lease_name(&e.name) else {
            continue;
        };
        // A file that cannot be read (permissions, a directory of that name) is an invalid lease.
        match read_lease(fs, &dir.join(&e.name)) {
            Ok(b) => out.push((id, LeaseSlot::decode_file(&b, store_id))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => out.push((id, None)),
        }
    }
    Ok(out)
}

/// The highest epoch any valid lease file in `dir` names, void ones included; 0 if none. New manifest epochs after
/// a start continue above it (`spec.md` §7 Manifest, `format.md` A.14).
pub fn highest_epoch(fs: &dyn Fs, dir: &Path, store_id: &[u8; 16]) -> io::Result<u64> {
    Ok(read_leases(fs, dir, store_id)?
        .into_iter()
        .filter_map(|(_, l)| l.map(|l| l.epoch))
        .max()
        .unwrap_or(0))
}

/// What the writer makes of one lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LeaseStatus {
    /// Honoured: its epoch is kept.
    Live,
    /// Of another boot: void.
    Void,
    /// Its counter has not changed for [`LEASE_EXPIRY`] since first observed.
    Expired,
}

/// The writer's observation of one lease file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Observed {
    /// The slot, `None` while the file is invalid (an invalid lease counts as holding epoch 0 until it expires).
    pub slot: Option<LeaseSlot>,
    /// When the writer first observed this value of the counter (an invalid file is one value).
    pub first_observed: Duration,
}

impl Observed {
    /// The epoch the lease holds: 0 while invalid.
    pub fn epoch(&self) -> u64 {
        self.slot.map_or(0, |s| s.epoch)
    }
}

/// The lease files as the writer last saw them (`spec.md` §7 Leases).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeaseTable {
    boot_id: [u8; 16],
    seen: BTreeMap<[u8; 16], Observed>,
    scanned_at: Option<Duration>,
}

impl LeaseTable {
    /// An empty table for a writer of boot `boot_id` (zero where the platform has none).
    pub fn new(boot_id: [u8; 16]) -> Self {
        Self {
            boot_id,
            seen: BTreeMap::new(),
            scanned_at: None,
        }
    }

    /// Whether a look at the lease files is due at `now` ([`LEASE_SCAN_INTERVAL`]).
    pub fn scan_due(&self, now: Duration) -> bool {
        self.scanned_at
            .is_none_or(|t| now.saturating_sub(t) >= LEASE_SCAN_INTERVAL)
    }

    /// The status of an observation at `now`. A zero boot id, the lease's or the writer's, is honoured until expiry.
    pub fn status(&self, o: &Observed, now: Duration) -> LeaseStatus {
        if let Some(s) = o.slot
            && s.boot_id != [0; 16]
            && self.boot_id != [0; 16]
            && s.boot_id != self.boot_id
        {
            return LeaseStatus::Void;
        }
        if now.saturating_sub(o.first_observed) >= LEASE_EXPIRY {
            LeaseStatus::Expired
        } else {
            LeaseStatus::Live
        }
    }

    /// Reads every lease file in `dir`, records a counter value not seen before as first observed at `now` (at the
    /// writer's start, every lease counts as first observed then), and deletes void and expired leases. Returns the
    /// reader ids deleted. A deletion that fails is tried again at the next look.
    pub fn scan(
        &mut self,
        fs: &dyn Fs,
        dir: &Path,
        store_id: &[u8; 16],
        now: Duration,
    ) -> io::Result<Vec<[u8; 16]>> {
        self.scanned_at = Some(now);
        let found = read_leases(fs, dir, store_id)?;
        let mut next = BTreeMap::new();
        for (id, slot) in found {
            let counter = slot.map(|s| s.refresh_counter);
            let o = match self.seen.get(&id) {
                Some(o) if o.slot.map(|s| s.refresh_counter) == counter => Observed {
                    slot,
                    first_observed: o.first_observed,
                },
                _ => Observed {
                    slot,
                    first_observed: now,
                },
            };
            next.insert(id, o);
        }
        self.seen = next;
        let mut removed = Vec::new();
        let gone: Vec<[u8; 16]> = self
            .seen
            .iter()
            .filter(|(_, o)| self.status(o, now) != LeaseStatus::Live)
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            match fs.remove_file(&dir.join(lease_name(&id))) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(_) => continue,
            }
            self.seen.remove(&id);
            removed.push(id);
        }
        Ok(removed)
    }

    /// The leases seen at the last look and not deleted, by reader id.
    pub fn observed(&self) -> &BTreeMap<[u8; 16], Observed> {
        &self.seen
    }

    /// The lowest epoch a live lease names at `now`, an invalid lease counting as epoch 0; `None` without a live
    /// lease. Removing a retired file needs no live lease naming an older epoch (`spec.md` §7 Manifest).
    pub fn oldest_live_epoch(&self, now: Duration) -> Option<u64> {
        self.seen
            .values()
            .filter(|o| self.status(o, now) == LeaseStatus::Live)
            .map(Observed::epoch)
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let s = LeaseSlot {
            epoch: 0x0102,
            refresh_counter: 7,
            boot_id: [3; 16],
            created_at: -4,
        };
        let b = s.encode_file([5; 16], 2);
        // Offsets from the A.15 table.
        assert_eq!(b[4], kind::LEASE);
        assert_eq!(&b[64..72], &0x0102u64.to_le_bytes());
        assert_eq!(&b[72..80], &7u64.to_le_bytes());
        assert_eq!(&b[80..96], &[3; 16]);
        assert_eq!(&b[96..104], &(-4i64).to_le_bytes());
        assert_eq!(&b[104..124], &[0; 20]);
        assert_eq!(&b[124..128], &crc32c(&b[64..124]).to_le_bytes());
        assert_eq!(&b[64..], &s.encode()[..]);
        for i in 64..LEASE_LEN {
            let mut d = b;
            d[i] ^= 0x10;
            let r = LeaseSlot::decode_file(&d, &[5; 16]);
            // Reserved bytes are covered by the checksum too.
            assert_eq!(r, None, "byte {i}");
        }
        assert_eq!(LeaseSlot::decode_file(&b[..127], &[5; 16]), None);
    }

    #[test]
    fn names_are_exact() {
        assert_eq!(lease_name(&[0; 16]), format!("{}.lease", "0".repeat(32)));
        assert_eq!(parse_lease_name(&format!("{}.lease", "A".repeat(32))), None);
        assert_eq!(parse_lease_name(&format!("{}.lease", "a".repeat(31))), None);
        assert_eq!(
            parse_lease_name(&format!("{}.lease.tmp", "a".repeat(32))),
            None
        );
    }
}
