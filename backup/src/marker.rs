//! The last-uploaded marker (`format.md` A.17 Last-uploaded marker; Q-614): put after each record a generation's
//! uploads put, it names the last record uploaded and the records a restore of the uploaded records reads, so a
//! restore that cannot reach them fails instead of reading an earlier state. Objects are never overwritten, so the
//! markers of a generation take dense sequence numbers from 1 and the latest is found by `stat` past the listing.

use kvik_files::record::{Body, Record};
use kvik_log::crc::crc32c;
use kvik_sink_api::{BackupSink, SinkError};
use sha2::{Digest, Sha256};

use crate::envelope::{Envelope, EnvelopeError};
use crate::keys::{Keys, ObjectName};
use crate::uploaded::opens_manifest;

/// The plaintext's length.
pub const MARKER_LEN: usize = 60;
/// The plaintext's first bytes.
pub const MAGIC: [u8; 4] = *b"KVKM";
/// The format version this build writes and reads.
pub const VERSION: u16 = 1;

/// What the records uploaded in one generation amount to, in the terms a restore checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// The last record uploaded; 0 for none.
    pub epoch: u64,
    /// The first record of the latest manifest uploaded: the latest rebuild record that opens one, or with none the
    /// first record uploaded; 0 for none.
    pub open: u64,
    /// The first and last epoch of [`crate::plan::records_to_read`] over the records uploaded; `None` when it reads
    /// none.
    pub read: Option<(u64, u64)>,
}

/// A rebuilt manifest is whole once its closing empty record, one without field 100, is uploaded (Q-108).
fn closes(r: &Record) -> bool {
    matches!(r.body, Body::Empty) && r.old_epoch.is_none()
}

impl Summary {
    /// The summary of `records`, in epoch order.
    pub fn of<'a>(records: impl IntoIterator<Item = &'a Record>) -> Self {
        let mut s = Self::default();
        for r in records {
            s.apply(r);
        }
        s
    }

    /// Takes in the next record uploaded; one at or below `epoch` is already counted and changes nothing.
    pub fn apply(&mut self, r: &Record) {
        if r.epoch <= self.epoch {
            return;
        }
        if opens_manifest(r) {
            self.open = r.epoch;
        } else if self.epoch == 0 {
            self.open = r.epoch;
            self.read = Some((r.epoch, r.epoch));
        } else if let Some((first, _)) = self.read.filter(|(f, _)| *f == self.open) {
            self.read = Some((first, r.epoch));
        } else if closes(r) {
            self.read = Some((self.open, r.epoch));
        }
        self.epoch = r.epoch;
    }

    fn valid(&self) -> Result<(), &'static str> {
        if self.epoch == 0 || self.open == 0 || self.open > self.epoch {
            return Err("a marker whose epochs are out of order");
        }
        match self.read {
            Some((f, l))
                if f == 0 || f > l || l > self.epoch || (f != self.open && l >= self.open) =>
            {
                Err("a marker whose read range is out of order")
            }
            _ => Ok(()),
        }
    }
}

/// One marker object's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Marker {
    /// The generation whose records it names, that of its key.
    pub generation: u32,
    /// Its sequence number, that of its key.
    pub sequence: u64,
    /// The records uploaded when it was put.
    pub summary: Summary,
}

/// Why a marker's plaintext is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MarkerError {
    /// Not exactly [`MARKER_LEN`] bytes.
    #[error("a marker of {0} bytes")]
    Length(usize),
    /// The magic is wrong.
    #[error("a marker without its magic")]
    Magic,
    /// The checksum fails.
    #[error("a marker whose checksum fails")]
    Checksum,
    /// A version this build does not read.
    #[error("a marker of version {0}")]
    Version(u16),
    /// Fields that cannot hold together.
    #[error("{0}")]
    Invalid(&'static str),
}

fn u64_at(b: &[u8; MARKER_LEN], at: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(x)
}

fn u32_at(b: &[u8; MARKER_LEN], at: usize) -> u32 {
    let mut x = [0u8; 4];
    x.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(x)
}

impl Marker {
    /// The plaintext to seal (format A.17).
    pub fn encode(&self) -> Vec<u8> {
        let s = &self.summary;
        let (first, last) = s.read.unwrap_or((0, 0));
        let mut b = Vec::with_capacity(MARKER_LEN);
        b.extend_from_slice(&MAGIC);
        b.extend_from_slice(&VERSION.to_le_bytes());
        b.extend_from_slice(&[0; 2]);
        b.extend_from_slice(&self.generation.to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        for x in [self.sequence, s.epoch, s.open, first, last] {
            b.extend_from_slice(&x.to_le_bytes());
        }
        let crc = crc32c(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }

    /// A marker from exactly its plaintext.
    pub fn decode(b: &[u8]) -> Result<Self, MarkerError> {
        let b: &[u8; MARKER_LEN] = b.try_into().map_err(|_| MarkerError::Length(b.len()))?;
        if b[..4] != MAGIC {
            return Err(MarkerError::Magic);
        }
        if crc32c(&b[..56]) != u32_at(b, 56) {
            return Err(MarkerError::Checksum);
        }
        let version = u16::from_le_bytes([b[4], b[5]]);
        if version != VERSION {
            return Err(MarkerError::Version(version));
        }
        if b[6..8] != [0; 2] || b[12..16] != [0; 4] {
            return Err(MarkerError::Invalid("a marker with reserved bytes set"));
        }
        let (first, last) = (u64_at(b, 40), u64_at(b, 48));
        let read = match (first, last) {
            (0, 0) => None,
            (0, _) => {
                return Err(MarkerError::Invalid(
                    "a marker whose read range is out of order",
                ));
            }
            r => Some(r),
        };
        let m = Self {
            generation: u32_at(b, 8),
            sequence: u64_at(b, 16),
            summary: Summary {
                epoch: u64_at(b, 24),
                open: u64_at(b, 32),
                read,
            },
        };
        if m.sequence == 0 {
            return Err(MarkerError::Invalid("a marker of sequence 0"));
        }
        m.summary.valid().map_err(MarkerError::Invalid)?;
        Ok(m)
    }
}

/// Why a marker could not be read.
#[derive(Debug)]
pub enum ReadError {
    /// The sink failed.
    Sink(SinkError),
    /// The object does not open, does not decode, or names another generation or sequence than its key.
    Unreadable(String),
}

/// Why a marker could not be put.
#[derive(Debug)]
pub enum PutError {
    /// The sink failed.
    Sink(SinkError),
    /// Sealing failed.
    Envelope(EnvelopeError),
}

/// Whether `stat` reports marker `sequence` of `g`.
async fn exists<S: BackupSink + ?Sized>(
    sink: &S,
    keys: &Keys,
    g: u32,
    sequence: u64,
) -> Result<bool, SinkError> {
    Ok(sink.stat(&keys.marker(g, sequence)).await?.is_some())
}

/// The highest sequence of `g`'s markers in the bucket, 0 for none: the highest listed, then `stat` past it, doubling
/// the step and halving back, since a listing can lag and the sequence has no gaps.
pub async fn latest<S: BackupSink + ?Sized>(
    sink: &S,
    keys: &Keys,
    g: u32,
) -> Result<u64, SinkError> {
    let prefix = keys.markers_prefix(g);
    let mut lo = 0u64;
    let mut after: Option<String> = None;
    loop {
        let page = sink.list(&prefix, after.as_deref()).await?;
        for k in &page.keys {
            if let Some(ObjectName::Marker {
                generation,
                sequence,
            }) = keys.parse(k)
                && generation == g
            {
                lo = lo.max(sequence);
            }
        }
        match page.keys.last() {
            Some(last) if page.more => after = Some(last.clone()),
            _ => break,
        }
    }
    let mut step = 1u64;
    let hi = loop {
        let Some(probe) = lo.checked_add(step) else {
            break u64::MAX;
        };
        if !exists(sink, keys, g, probe).await? {
            break probe;
        }
        lo = probe;
        step = step.saturating_mul(2);
    };
    let mut hi = hi;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if exists(sink, keys, g, mid).await? {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// Marker `sequence` of `g`, opened, decoded and checked against its key.
pub async fn read<S: BackupSink + ?Sized, E: Envelope + ?Sized>(
    sink: &S,
    env: &E,
    keys: &Keys,
    g: u32,
    sequence: u64,
) -> Result<Marker, ReadError> {
    let key = keys.marker(g, sequence);
    let bytes = sink.get(&key, None).await.map_err(ReadError::Sink)?;
    let plain = env
        .open(&key, &bytes)
        .map_err(|e| ReadError::Unreadable(e.to_string()))?;
    let m = Marker::decode(&plain).map_err(|e| ReadError::Unreadable(e.to_string()))?;
    if (m.generation, m.sequence) != (g, sequence) {
        return Err(ReadError::Unreadable(
            "a marker naming another generation or sequence than its key".into(),
        ));
    }
    Ok(m)
}

/// Puts `summary` as marker `from` of `g`, or the next sequence free where other markers hold the earlier ones;
/// returns the sequence it took.
pub async fn put<S: BackupSink + ?Sized, E: Envelope + ?Sized>(
    sink: &S,
    env: &E,
    keys: &Keys,
    g: u32,
    from: u64,
    summary: Summary,
) -> Result<u64, PutError> {
    let mut sequence = from.max(1);
    loop {
        let m = Marker {
            generation: g,
            sequence,
            summary,
        };
        let key = keys.marker(g, sequence);
        let sealed = env.seal(&key, &m.encode()).map_err(PutError::Envelope)?;
        let sha: [u8; 32] = Sha256::digest(&sealed).into();
        match sink.put(&key, sealed.into(), sha).await {
            Ok(_) => return Ok(sequence),
            Err(SinkError::Exists { .. }) => {
                sequence = sequence.checked_add(1).ok_or_else(|| {
                    PutError::Sink(SinkError::Failed {
                        reason: format!("no marker sequence is left in generation {g}"),
                    })
                })?;
            }
            Err(e) => return Err(PutError::Sink(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use kvik_files::record::Rebuild;

    use super::*;
    use crate::plan::records_to_read;

    fn rec(epoch: u64, body: Body) -> Record {
        Record::new(epoch, 0, 1, body)
    }

    fn range(records: &[Record]) -> Option<(u64, u64)> {
        let r = records_to_read(records);
        Some((r.first()?.epoch, r.last()?.epoch))
    }

    /// Format A.17 by hand: magic `KVKM`, version 1, reserved, generation 2, reserved, sequence 3, epoch 9, open 5,
    /// read 5 to 9; then the CRC32C of those 56 bytes, `0xcc5841c1`.
    const VECTOR: &str = concat!(
        "4b564b4d",
        "0100",
        "0000",
        "02000000",
        "00000000",
        "0300000000000000",
        "0900000000000000",
        "0500000000000000",
        "0500000000000000",
        "0900000000000000",
    );

    fn vector() -> Marker {
        Marker {
            generation: 2,
            sequence: 3,
            summary: Summary {
                epoch: 9,
                open: 5,
                read: Some((5, 9)),
            },
        }
    }

    #[test]
    fn a_marker_is_the_format_vector_then_its_checksum() {
        let b = vector().encode();
        assert_eq!(b.len(), MARKER_LEN);
        let hex: String = b[..56].iter().map(|x| format!("{x:02x}")).collect();
        assert_eq!(hex, VECTOR);
        assert_eq!(b[56..], crc32c(&b[..56]).to_le_bytes());
        assert_eq!(b[56..], [0xc1, 0x41, 0x58, 0xcc]);
        assert_eq!(Marker::decode(&b), Ok(vector()));
    }

    #[test]
    fn a_marker_with_no_read_range_writes_zeros_there() {
        let m = Marker {
            summary: Summary {
                epoch: 7,
                open: 5,
                read: None,
            },
            ..vector()
        };
        let b = m.encode();
        assert_eq!(b[40..56], [0; 16]);
        assert_eq!(Marker::decode(&b), Ok(m));
    }

    #[test]
    fn a_damaged_or_foreign_marker_is_refused_with_its_reason() {
        let b = vector().encode();
        assert_eq!(Marker::decode(&b[..59]), Err(MarkerError::Length(59)));
        assert_eq!(
            Marker::decode(&[b.as_slice(), &[0]].concat()),
            Err(MarkerError::Length(61))
        );
        let mut magic = b.clone();
        magic[0] ^= 1;
        assert_eq!(Marker::decode(&magic), Err(MarkerError::Magic));
        let mut flipped = b.clone();
        flipped[30] ^= 1;
        assert_eq!(Marker::decode(&flipped), Err(MarkerError::Checksum));
        let reseal = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut v = b[..56].to_vec();
            f(&mut v);
            let crc = crc32c(&v);
            v.extend_from_slice(&crc.to_le_bytes());
            Marker::decode(&v)
        };
        assert_eq!(reseal(&|v| v[4] = 2), Err(MarkerError::Version(2)));
        for (at, why) in [
            (6, "a marker with reserved bytes set"),
            (12, "a marker with reserved bytes set"),
        ] {
            assert_eq!(reseal(&|v| v[at] = 1), Err(MarkerError::Invalid(why)));
        }
        let field = |at: usize, x: u64| {
            move |v: &mut Vec<u8>| v[at..at + 8].copy_from_slice(&x.to_le_bytes())
        };
        for (at, x) in [
            (16, 0),
            (24, 0),
            (24, 8),
            (32, 0),
            (32, 10),
            (40, 0),
            (40, 6),
            (48, 10),
            (48, 4),
        ] {
            assert!(
                matches!(reseal(&field(at, x)), Err(MarkerError::Invalid(_))),
                "{at} {x}"
            );
        }
    }

    #[test]
    fn the_summary_reads_what_a_restore_of_the_records_uploaded_reads() {
        let mut v = vec![
            rec(1, Body::Empty),
            rec(2, Body::Empty),
            rec(3, Body::Empty),
        ];
        let check = |v: &[Record]| {
            let s = Summary::of(v);
            assert_eq!(s.read, range(v), "{v:?}");
            assert_eq!(s.epoch, v.last().unwrap().epoch);
        };
        check(&v);
        v.push(rec(5, Body::Rebuild(Rebuild::default())));
        check(&v);
        v.push(Record {
            old_epoch: Some(1),
            ..rec(6, Body::Empty)
        });
        check(&v);
        v.push(rec(9, Body::Empty));
        check(&v);
        assert_eq!(Summary::of(&v).read, Some((5, 9)));
        v.push(rec(10, Body::Empty));
        check(&v);
        v.push(rec(12, Body::Rebuild(Rebuild::default())));
        v.push(rec(13, Body::Empty));
        check(&v);
        assert_eq!(Summary::of(&v).read, Some((12, 13)));
        let unfinished = [rec(5, Body::Rebuild(Rebuild::default()))];
        assert_eq!(
            Summary::of(&unfinished),
            Summary {
                epoch: 5,
                open: 5,
                read: None
            }
        );
    }

    #[test]
    fn a_record_already_counted_changes_nothing() {
        let v = [rec(1, Body::Empty), rec(4, Body::Empty)];
        let mut s = Summary::of(&v);
        let before = s;
        s.apply(&rec(4, Body::Rebuild(Rebuild::default())));
        s.apply(&rec(2, Body::Empty));
        assert_eq!(s, before);
    }
}
