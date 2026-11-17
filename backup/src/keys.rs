//! Object keys in the bucket: the only place one is built or parsed. Provisional layout (Q-198 (b), (e): Q-160's
//! suggestion with no engine prefix; keys start at the store id, and a restore finds the store id by listing):
//!
//! ```text
//! <store_id:32 hex>/STORE
//! <store_id>/probe
//! <store_id>/probe/<nonce:32 hex>                                 a checksum probe (Q-213)
//! <store_id>/log/<generation:8 hex>-<file_id:16 hex>.kvl        a log segment
//! <store_id>/files/<kind:2 hex>/<generation:8 hex>-<file_id:16 hex>   any other kind
//! <store_id>/manifest/<generation:8 hex>/<epoch:16 hex>
//! <store_id>/claims/<generation:8 hex>
//! ```

use std::fmt::Write as _;

use kvik_files::desc::{FileDescription, FileRef, kind};

/// What an object key names.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjectName {
    /// The store descriptor.
    Store,
    /// A file, with the generation it was created in.
    File {
        /// The file.
        file: FileRef,
        /// Its generation.
        generation: u32,
    },
    /// A manifest record.
    Record {
        /// The generation it was written in.
        generation: u32,
        /// Its epoch.
        epoch: u64,
    },
    /// A generation's claim marker.
    Claim(u32),
    /// The attach probe.
    Probe,
    /// A checksum probe, with its nonce (Q-213).
    ChecksumProbe([u8; 16]),
}

/// The keys of one store's objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keys {
    store_id: [u8; 16],
    root: String,
}

impl Keys {
    /// The keys of store `store_id`.
    pub fn new(store_id: [u8; 16]) -> Self {
        let mut root = String::with_capacity(33);
        push_hex(&mut root, &store_id);
        root.push('/');
        Self { store_id, root }
    }

    /// The store id.
    pub fn store_id(&self) -> [u8; 16] {
        self.store_id
    }

    /// The store descriptor's key.
    pub fn store(&self) -> String {
        format!("{}STORE", self.root)
    }

    /// The attach probe's key.
    pub fn probe(&self) -> String {
        format!("{}probe", self.root)
    }

    /// A checksum probe's key under a fresh `nonce` (format A.17 Probe; Q-213).
    pub fn checksum_probe(&self, nonce: [u8; 16]) -> String {
        let mut k = format!("{}probe/", self.root);
        push_hex(&mut k, &nonce);
        k
    }

    /// A file's key.
    pub fn file(&self, d: &FileDescription) -> String {
        let (g, f) = (d.generation, d.file_id);
        match d.kind {
            kind::LOG_SEGMENT => format!("{}log/{g:08x}-{f:016x}.kvl", self.root),
            k => format!("{}files/{k:02x}/{g:08x}-{f:016x}", self.root),
        }
    }

    /// The prefix of a generation's log segments.
    pub fn log_prefix(&self, generation: u32) -> String {
        format!("{}log/{generation:08x}-", self.root)
    }

    /// The prefix of files of every kind but log segments.
    pub fn files_prefix(&self) -> String {
        format!("{}files/", self.root)
    }

    /// A generation's claim marker's key.
    pub fn claim(&self, generation: u32) -> String {
        format!("{}claims/{generation:08x}", self.root)
    }

    /// A manifest record's key.
    pub fn record(&self, generation: u32, epoch: u64) -> String {
        format!("{}{epoch:016x}", self.records_prefix(generation))
    }

    /// The prefix of a generation's records; a listing returns them in epoch order.
    pub fn records_prefix(&self, generation: u32) -> String {
        format!("{}manifest/{generation:08x}/", self.root)
    }

    /// What `key` names, if it is one of this store's keys.
    pub fn parse(&self, key: &str) -> Option<ObjectName> {
        let rest = key.strip_prefix(self.root.as_str())?;
        match rest {
            "STORE" => return Some(ObjectName::Store),
            "probe" => return Some(ObjectName::Probe),
            _ => {}
        }
        let (dir, rest) = rest.split_once('/')?;
        match dir {
            "log" => {
                let (generation, file_id) = file_stem(rest.strip_suffix(".kvl")?)?;
                Some(ObjectName::File {
                    file: FileRef {
                        kind: kind::LOG_SEGMENT,
                        file_id,
                    },
                    generation,
                })
            }
            "files" => {
                let (k, stem) = rest.split_once('/')?;
                let k = hex(k, 2)? as u8;
                if k == kind::LOG_SEGMENT {
                    return None;
                }
                let (generation, file_id) = file_stem(stem)?;
                Some(ObjectName::File {
                    file: FileRef { kind: k, file_id },
                    generation,
                })
            }
            "manifest" => {
                let (g, e) = rest.split_once('/')?;
                Some(ObjectName::Record {
                    generation: hex(g, 8)? as u32,
                    epoch: hex(e, 16)?,
                })
            }
            "claims" => Some(ObjectName::Claim(hex(rest, 8)? as u32)),
            "probe" => Some(ObjectName::ChecksumProbe(bytes16(rest)?)),
            _ => None,
        }
    }

    /// The store id a key starts with; a restore lists `""` and reads the store ids from the keys.
    pub fn store_id_of(key: &str) -> Option<[u8; 16]> {
        bytes16(key.split_once('/')?.0)
    }
}

fn push_hex(s: &mut String, bytes: &[u8]) {
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
}

/// 16 bytes from exactly 32 lower-case hex digits.
fn bytes16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = hex(s.get(2 * i..2 * i + 2)?, 2)? as u8;
    }
    Some(out)
}

fn file_stem(s: &str) -> Option<(u32, u64)> {
    let (g, f) = s.split_once('-')?;
    Some((hex(g, 8)? as u32, hex(f, 16)?))
}

/// Lower-case hex of exactly `width` digits.
fn hex(s: &str, width: usize) -> Option<u64> {
    let ok = s.len() == width && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    ok.then(|| u64::from_str_radix(s, 16).ok()).flatten()
}

/// A generation's claim marker (Q-198 (f), Q-161's suggestion): the claiming restore's reset-stamp nonce, so a `put` that
/// succeeds proves the marker is this restore's. Provisional bytes: `generation: u32`, `nonce: u64`, little-endian.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimMarker {
    /// The generation claimed.
    pub generation: u32,
    /// The restore's reset-stamp nonce.
    pub nonce: u64,
}

impl ClaimMarker {
    /// Its bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(12);
        b.extend_from_slice(&self.generation.to_le_bytes());
        b.extend_from_slice(&self.nonce.to_le_bytes());
        b
    }

    /// A marker from exactly its bytes.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let g: [u8; 4] = b.get(..4)?.try_into().ok()?;
        let n: [u8; 8] = b.get(4..)?.try_into().ok()?;
        Some(Self {
            generation: u32::from_le_bytes(g),
            nonce: u64::from_le_bytes(n),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvik_files::desc::FileClass;

    const ID: [u8; 16] = [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0, 1, 2, 3, 4, 5, 6, 0xff,
    ];
    const ROOT: &str = "0123456789abcdef00010203040506ff/";

    fn desc(kind: u8, generation: u32, file_id: u64) -> FileDescription {
        FileDescription {
            kind,
            class: FileClass::None,
            generation,
            file_id,
            range: None,
            hash: [0; 32],
            inputs: Vec::new(),
        }
    }

    #[test]
    fn keys_follow_the_provisional_layout() {
        let k = Keys::new(ID);
        assert_eq!(k.store_id(), ID);
        assert_eq!(k.store(), format!("{ROOT}STORE"));
        assert_eq!(k.probe(), format!("{ROOT}probe"));
        assert_eq!(
            k.checksum_probe(ID),
            format!("{ROOT}probe/0123456789abcdef00010203040506ff")
        );
        assert_eq!(
            k.file(&desc(kind::LOG_SEGMENT, 2, 0xab)),
            format!("{ROOT}log/00000002-00000000000000ab.kvl")
        );
        assert_eq!(
            k.file(&desc(kind::COMPACTED, 1, 7)),
            format!("{ROOT}files/02/00000001-0000000000000007")
        );
        assert_eq!(k.claim(3), format!("{ROOT}claims/00000003"));
        assert_eq!(
            k.record(1, 0x10),
            format!("{ROOT}manifest/00000001/0000000000000010")
        );
        assert!(k.record(1, 0x10).starts_with(&k.records_prefix(1)));
        assert!(!k.record(1, 0x10).starts_with(&k.records_prefix(0)));
    }

    #[test]
    fn records_list_in_epoch_order() {
        let k = Keys::new(ID);
        let mut keys: Vec<String> = [300u64, 2, 0x1_0000, 17].map(|e| k.record(0, e)).into();
        keys.sort();
        let epochs: Vec<u64> = keys
            .iter()
            .map(|s| match k.parse(s) {
                Some(ObjectName::Record { epoch, .. }) => epoch,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(epochs, [2, 17, 300, 0x1_0000]);
    }

    #[test]
    fn every_built_key_parses_back_to_its_name() {
        let k = Keys::new(ID);
        let file = |kd: u8, g: u32, f: u64| ObjectName::File {
            file: FileRef {
                kind: kd,
                file_id: f,
            },
            generation: g,
        };
        let cases = [
            (k.store(), ObjectName::Store),
            (k.probe(), ObjectName::Probe),
            (
                k.checksum_probe([0xa5; 16]),
                ObjectName::ChecksumProbe([0xa5; 16]),
            ),
            (
                k.file(&desc(kind::LOG_SEGMENT, u32::MAX, u64::MAX)),
                file(kind::LOG_SEGMENT, u32::MAX, u64::MAX),
            ),
            (k.file(&desc(0xfe, 0, 1)), file(0xfe, 0, 1)),
            (k.claim(u32::MAX), ObjectName::Claim(u32::MAX)),
            (
                k.record(5, u64::MAX),
                ObjectName::Record {
                    generation: 5,
                    epoch: u64::MAX,
                },
            ),
        ];
        for (key, name) in cases {
            assert_eq!(k.parse(&key), Some(name), "{key}");
            assert_eq!(Keys::store_id_of(&key), Some(ID), "{key}");
        }
    }

    #[test]
    fn foreign_and_malformed_keys_do_not_parse() {
        let k = Keys::new(ID);
        let other = Keys::new([0; 16]);
        assert_eq!(k.parse(&other.store()), None);
        for bad in [
            format!("{ROOT}store"),
            format!("{ROOT}log/00000002-00000000000000AB.kvl"),
            format!("{ROOT}log/00000002-00000000000000ab.open"),
            format!("{ROOT}log/2-ab.kvl"),
            format!("{ROOT}files/01/00000001-0000000000000007"),
            format!("{ROOT}files/2/00000001-0000000000000007"),
            format!("{ROOT}manifest/00000001/10"),
            format!("{ROOT}manifest/00000001/0000000000000010/x"),
            format!("{ROOT}claims/0000000g"),
            format!("{ROOT}claims/+0000003"),
            format!("{ROOT}other/x"),
            format!("{ROOT}probe/0123456789ABCDEF00010203040506ff"),
            format!("{ROOT}probe/0123456789abcdef00010203040506f"),
            format!("{ROOT}probe/"),
            "STORE".to_owned(),
        ] {
            assert_eq!(k.parse(&bad), None, "{bad}");
        }
        assert_eq!(Keys::store_id_of("STORE"), None);
        assert_eq!(
            Keys::store_id_of("0123456789ABCDEF00010203040506ff/STORE"),
            None
        );
        assert_eq!(
            Keys::store_id_of("0123456789abcdef00010203040506f/STORE"),
            None
        );
    }

    #[test]
    fn claim_marker_round_trips_and_refuses_other_lengths() {
        let m = ClaimMarker {
            generation: 0x0102_0304,
            nonce: 0x1122_3344_5566_7788,
        };
        let b = m.encode();
        assert_eq!(
            b,
            [4, 3, 2, 1, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]
        );
        assert_eq!(ClaimMarker::decode(&b), Some(m));
        assert_eq!(ClaimMarker::decode(&b[..11]), None);
        assert_eq!(ClaimMarker::decode(&[b.as_slice(), &[0]].concat()), None);
        assert_eq!(ClaimMarker::decode(&[]), None);
    }
}
