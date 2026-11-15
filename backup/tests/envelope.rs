//! The envelope of `format.md` A.17: round trip, ranges, determinism, tamper evidence, the key check, object-key
//! binding, an independent construction from the format text, and golden objects.

use std::fs;
use std::path::PathBuf;

use chacha20poly1305::{AeadInOut, KeyInit, XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use kvik_backup::envelope::{CHUNK, ChunkedXChaCha, Envelope, EnvelopeError};
use kvik_backup::keys::Keys;
use kvik_log::crc::crc32c;
use proptest::prelude::*;
use sha2::{Digest, Sha256};

const KEY: [u8; 32] = [0x42; 32];
const SID: [u8; 16] = [0x03; 16];
const HEADER: usize = 64;
const FRAME: usize = CHUNK + 16;

fn envelope() -> ChunkedXChaCha {
    ChunkedXChaCha::new(KEY, SID)
}

fn object_key() -> String {
    Keys::new(SID).probe()
}

/// `len` bytes from a xorshift seeded by `seed`.
fn plain(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Lengths weighted to the small, with every chunk boundary up to four chunks.
fn length() -> impl Strategy<Value = usize> {
    prop_oneof![
        3 => 0..300usize,
        2 => (0..5usize, 0..5usize).prop_map(|(k, d)| (k * CHUNK + d).saturating_sub(2)),
        1 => 0..4 * CHUNK,
    ]
}

fn chunk_count(len: usize) -> usize {
    len.div_ceil(CHUNK).max(1)
}

/// The object bytes `span` names, clamped to the object, as a ranged read returns them.
fn fetch<'a>(e: &ChunkedXChaCha, object: &'a [u8], plain: std::ops::Range<u64>) -> &'a [u8] {
    let s = e.span(plain);
    let n = object.len() as u64;
    &object[s.start.min(n) as usize..s.end.min(n) as usize]
}

/// A.17 written out again from the format text alone, with the primitives called directly.
fn reference_seal(key: [u8; 32], sid: [u8; 16], object_key: &str, p: &[u8]) -> Vec<u8> {
    let mac = |k: &[u8], parts: &[&[u8]]| -> [u8; 32] {
        let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(k).unwrap();
        for x in parts {
            m.update(x);
        }
        m.finalize().into_bytes().into()
    };
    let e = mac(&key, &[b"kvik envelope 1 encryption", &sid]);
    let n_key = mac(&key, &[b"kvik envelope 1 nonce", &sid]);
    let kcv = mac(&key, &[b"kvik envelope 1 key check", &sid]);
    let digest: [u8; 32] = Sha256::digest(p).into();
    let base = mac(&n_key, &[object_key.as_bytes(), &digest]);
    let mut h = vec![0u8; 64];
    h[..4].copy_from_slice(b"KVKE");
    h[4] = 1;
    h[5] = 1;
    h[8..16].copy_from_slice(&(p.len() as u64).to_le_bytes());
    h[16..24].copy_from_slice(&kcv[..8]);
    h[24..43].copy_from_slice(&base[..19]);
    let crc = crc32c(&h[..60]);
    h[60..].copy_from_slice(&crc.to_le_bytes());
    let cipher = XChaCha20Poly1305::new(&e.into());
    let aad = [h.as_slice(), object_key.as_bytes()].concat();
    let n = chunk_count(p.len());
    let mut out = h.clone();
    for i in 0..n {
        let mut nonce = [0u8; 24];
        nonce[..19].copy_from_slice(&base[..19]);
        nonce[19..23].copy_from_slice(&(i as u32).to_le_bytes());
        nonce[23] = u8::from(i == n - 1);
        let mut c = p[(i * CHUNK).min(p.len())..((i + 1) * CHUNK).min(p.len())].to_vec();
        let tag = cipher
            .encrypt_inout_detached(&XNonce::from(nonce), &aad, c.as_mut_slice().into())
            .unwrap();
        out.extend_from_slice(&c);
        out.extend_from_slice(&tag);
    }
    out
}

#[test]
fn sealing_equals_the_format_text_built_independently() {
    let e = envelope();
    for len in [0, 1, 300, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 5] {
        let p = plain(len, len as u64);
        let k = object_key();
        assert_eq!(
            e.seal(&k, &p).unwrap(),
            reference_seal(KEY, SID, &k, &p),
            "{len}"
        );
    }
}

#[test]
fn object_size_and_chunk_count_follow_the_format() {
    let e = envelope();
    for (len, n) in [
        (0, 1),
        (1, 1),
        (CHUNK, 1),
        (CHUNK + 1, 2),
        (2 * CHUNK, 2),
        (2 * CHUNK + 1, 3),
    ] {
        let o = e.seal("k", &plain(len, 9)).unwrap();
        assert_eq!(o.len(), HEADER + len + 16 * n, "{len}");
        assert_eq!(chunk_count(len), n);
    }
    assert_eq!(e.header_len(), HEADER as u64);
}

#[test]
fn span_names_whole_frames_and_never_panics() {
    let e = envelope();
    let c = CHUNK as u64;
    let f = FRAME as u64;
    assert_eq!(e.span(0..1), 64..64 + f);
    assert_eq!(e.span(0..c), 64..64 + f);
    assert_eq!(e.span(c - 1..c + 1), 64..64 + 2 * f);
    assert_eq!(e.span(c..2 * c), 64 + f..64 + 2 * f);
    assert_eq!(e.span(5..5), 64..64);
    #[allow(clippy::reversed_empty_ranges)]
    let inverted = 9..3;
    assert_eq!(e.span(inverted), 64..64);
    let s = e.span(u64::MAX - 1..u64::MAX);
    assert!(s.start <= s.end);
}

#[test]
fn key_check_is_the_derived_value_and_differs_by_key_and_store() {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(&KEY).unwrap();
    m.update(b"kvik envelope 1 key check");
    m.update(&SID);
    let want: [u8; 32] = m.finalize().into_bytes().into();
    assert_eq!(envelope().key_check().0, want[..8]);
    assert_ne!(
        envelope().key_check(),
        ChunkedXChaCha::new([0x43; 32], SID).key_check()
    );
    assert_ne!(
        envelope().key_check(),
        ChunkedXChaCha::new(KEY, [0x04; 16]).key_check()
    );
}

#[test]
fn debug_prints_no_key_bytes() {
    let s = format!("{:?}", ChunkedXChaCha::new([0xab; 32], SID));
    assert!(!s.contains("ab") && !s.contains("171"), "{s}");
}

#[test]
fn a_range_past_the_plaintext_or_inverted_is_refused() {
    let e = envelope();
    let o = e.seal("k", &plain(10, 1)).unwrap();
    #[allow(clippy::reversed_empty_ranges)]
    let inverted = 6..5;
    for r in [0..11, 11..12, inverted] {
        assert_eq!(
            e.open_span("k", &o[..HEADER], r.clone(), fetch(&e, &o, r)),
            Err(EnvelopeError::Truncated)
        );
    }
    assert_eq!(e.open_span("k", &o[..HEADER], 4..4, &[]), Ok(Vec::new()));
}

#[test]
fn a_damaged_header_is_damage_never_a_wrong_key_or_a_version_refusal() {
    let e = envelope();
    let o = e.seal("k", b"x").unwrap();
    let mut magic = o.clone();
    magic[0] ^= 1;
    let mut kcv = o.clone();
    kcv[16] ^= 1;
    let mut version = o.clone();
    version[4] = 2;
    for bad in [&magic, &kcv, &version] {
        assert_eq!(
            e.check_header(bad),
            Err(EnvelopeError::Authentication { chunk: 0 })
        );
    }
    let mut reserved = o.clone();
    reserved[50] = 1;
    let crc = crc32c(&reserved[..60]);
    reserved[60..64].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(
        e.check_header(&reserved),
        Err(EnvelopeError::Authentication { chunk: 0 })
    );
    let crc = crc32c(&version[..60]);
    version[60..64].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(
        e.check_header(&version),
        Err(EnvelopeError::Unsupported {
            version: 2,
            algorithm: 1
        })
    );
    assert_eq!(
        e.check_header(&o[..HEADER - 1]),
        Err(EnvelopeError::Truncated)
    );
}

#[test]
fn a_header_declaring_more_than_an_envelope_holds_is_too_large_without_allocating() {
    let e = envelope();
    let mut o = e.seal("k", b"x").unwrap();
    let size = (1u64 << 48) + 1;
    o[8..16].copy_from_slice(&size.to_le_bytes());
    let crc = crc32c(&o[..60]);
    o[60..64].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(e.open("k", &o), Err(EnvelopeError::TooLarge { size }));
    let at_limit = 1u64 << 48;
    o[8..16].copy_from_slice(&at_limit.to_le_bytes());
    let crc = crc32c(&o[..60]);
    o[60..64].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(e.open("k", &o), Err(EnvelopeError::Truncated));
}

proptest! {
    #[test]
    fn seal_then_open_round_trips(len in length(), seed in any::<u64>()) {
        let e = envelope();
        let p = plain(len, seed);
        let o = e.seal(&object_key(), &p).unwrap();
        prop_assert_eq!(o.len(), HEADER + len + 16 * chunk_count(len));
        prop_assert_eq!(e.check_header(&o[..HEADER]), Ok(()));
        prop_assert_eq!(e.open(&object_key(), &o).unwrap(), p);
    }

    #[test]
    fn a_ranged_open_equals_the_slice_of_the_whole(len in length(), seed in any::<u64>(), a in any::<prop::sample::Index>(), b in any::<prop::sample::Index>()) {
        let e = envelope();
        let p = plain(len, seed);
        let o = e.seal(&object_key(), &p).unwrap();
        let (a, b) = (a.index(len + 1), b.index(len + 1));
        let r = a.min(b) as u64..a.max(b) as u64;
        let got = e.open_span(&object_key(), &o[..HEADER], r.clone(), fetch(&e, &o, r.clone())).unwrap();
        prop_assert_eq!(got, &p[r.start as usize..r.end as usize]);
    }

    #[test]
    fn the_same_inputs_seal_to_the_same_bytes(len in length(), seed in any::<u64>()) {
        let p = plain(len, seed);
        let first = envelope().seal(&object_key(), &p).unwrap();
        prop_assert_eq!(&envelope().seal(&object_key(), &p).unwrap(), &first);
        prop_assert_eq!(&ChunkedXChaCha::new(KEY, SID).seal(&object_key(), &p).unwrap(), &first);
    }

    #[test]
    fn another_plaintext_or_object_key_gets_another_nonce_base(len in 1..3 * CHUNK, seed in any::<u64>(), at in any::<prop::sample::Index>()) {
        let e = envelope();
        let p = plain(len, seed);
        let mut q = p.clone();
        q[at.index(len)] ^= 1;
        let k = Keys::new(SID);
        let o = e.seal(&k.probe(), &p).unwrap();
        let base = &o[24..43];
        prop_assert_ne!(base, &e.seal(&k.probe(), &q).unwrap()[24..43]);
        prop_assert_ne!(base, &e.seal(&k.store(), &p).unwrap()[24..43]);
    }

    #[test]
    fn any_single_bit_flip_is_rejected(len in length(), seed in any::<u64>(), at in any::<prop::sample::Index>(), bit in 0..8u8) {
        let e = envelope();
        let p = plain(len, seed);
        let mut o = e.seal(&object_key(), &p).unwrap();
        let i = at.index(o.len());
        o[i] ^= 1 << bit;
        let whole = e.open(&object_key(), &o);
        prop_assert!(matches!(whole, Err(EnvelopeError::Authentication { .. })), "{whole:?}");
        if i >= HEADER {
            let chunk = ((i - HEADER) / FRAME) as u64;
            let c = CHUNK as u64;
            let r = chunk * c..((chunk + 1) * c).min(len as u64);
            if r.start < r.end {
                let part = e.open_span(&object_key(), &o[..HEADER], r.clone(), fetch(&e, &o, r));
                prop_assert_eq!(part, Err(EnvelopeError::Authentication { chunk }));
            }
        }
    }

    #[test]
    fn truncation_and_extension_are_rejected(len in length(), seed in any::<u64>(), cut in any::<prop::sample::Index>(), extra in proptest::collection::vec(any::<u8>(), 1..40)) {
        let e = envelope();
        let o = e.seal(&object_key(), &plain(len, seed)).unwrap();
        let n = cut.index(o.len());
        let short = e.open(&object_key(), &o[..n]);
        prop_assert_eq!(short, Err(EnvelopeError::Truncated));
        let long = [o.as_slice(), &extra].concat();
        prop_assert_eq!(e.open(&object_key(), &long), Err(EnvelopeError::Authentication { chunk: chunk_count(len) as u64 }));
    }

    #[test]
    fn reordered_or_transplanted_chunks_are_rejected(full in 2..4usize, tail in 1..300usize, seed in any::<u64>(), i in any::<prop::sample::Index>(), j in any::<prop::sample::Index>()) {
        let e = envelope();
        let len = full * CHUNK + tail;
        let p = plain(len, seed);
        let o = e.seal(&object_key(), &p).unwrap();
        let (i, j) = (i.index(full), j.index(full - 1));
        let j = if j >= i { j + 1 } else { j };
        let frame = |k: usize| HEADER + k * FRAME..HEADER + (k + 1) * FRAME;
        let mut swapped = o.clone();
        swapped[frame(i)].copy_from_slice(&o[frame(j)]);
        swapped[frame(j)].copy_from_slice(&o[frame(i)]);
        let first = i.min(j) as u64;
        prop_assert_eq!(e.open(&object_key(), &swapped), Err(EnvelopeError::Authentication { chunk: first }));
        let other = e.seal(&object_key(), &plain(len, !seed)).unwrap();
        let mut moved = o.clone();
        moved[frame(i)].copy_from_slice(&other[frame(i)]);
        prop_assert_eq!(e.open(&object_key(), &moved), Err(EnvelopeError::Authentication { chunk: i as u64 }));
        // The last full chunk as the final one: dropping the short tail, header patched to match.
        let mut dropped = o[..HEADER + full * FRAME].to_vec();
        dropped[8..16].copy_from_slice(&((full * CHUNK) as u64).to_le_bytes());
        let crc = crc32c(&dropped[..60]);
        dropped[60..64].copy_from_slice(&crc.to_le_bytes());
        prop_assert_eq!(e.open(&object_key(), &dropped), Err(EnvelopeError::Authentication { chunk: 0 }));
    }

    #[test]
    fn a_wrong_key_or_store_is_told_apart_before_any_chunk(len in length(), seed in any::<u64>(), other in any::<[u8; 32]>(), flip in 0..16usize) {
        let o = envelope().seal(&object_key(), &plain(len, seed)).unwrap();
        let mut other = other;
        if other == KEY { other[0] ^= 1; }
        let mut sid = SID;
        sid[flip] ^= 0x80;
        for wrong in [ChunkedXChaCha::new(other, SID), ChunkedXChaCha::new(KEY, sid)] {
            prop_assert_eq!(wrong.check_header(&o[..HEADER]), Err(EnvelopeError::WrongKey));
            prop_assert_eq!(wrong.open(&object_key(), &o[..HEADER]), Err(EnvelopeError::WrongKey));
            prop_assert_eq!(wrong.open(&object_key(), &o), Err(EnvelopeError::WrongKey));
            let r = 0..len as u64;
            prop_assert_eq!(wrong.open_span(&object_key(), &o[..HEADER], r.clone(), fetch(&wrong, &o, r)), Err(EnvelopeError::WrongKey));
        }
    }

    #[test]
    fn an_object_moved_to_another_key_is_rejected(len in length(), seed in any::<u64>(), g in any::<u32>(), epoch in any::<u64>()) {
        let e = envelope();
        let k = Keys::new(SID);
        let o = e.seal(&k.record(g, epoch), &plain(len, seed)).unwrap();
        for to in [k.record(g, epoch ^ 1), k.record(g ^ 1, epoch), k.store(), k.claim(g)] {
            prop_assert_eq!(e.open(&to, &o), Err(EnvelopeError::Authentication { chunk: 0 }));
            let r = 0..len as u64;
            if len > 0 {
                prop_assert_eq!(e.open_span(&to, &o[..HEADER], r.clone(), fetch(&e, &o, r)), Err(EnvelopeError::Authentication { chunk: 0 }));
            }
        }
    }
}

/// The golden objects: (file, object key, plaintext). Multi-chunk objects are pinned by SHA-256 in
/// [`GOLDEN_DIGESTS`] instead of committed bytes.
fn golden_cases() -> Vec<(&'static str, String, Vec<u8>)> {
    let k = Keys::new(SID);
    vec![
        ("empty.kve", k.store(), Vec::new()),
        (
            "short.kve",
            k.claim(1),
            (0..=255u8).chain(*b"kvik").collect(),
        ),
    ]
}

const GOLDEN_DIGESTS: [(usize, &str); 2] = [
    (
        CHUNK,
        "9f3dd65fdcb8cd0bca863b8827f3c9cc3708915bb12317b13ad98cc62ed250eb",
    ),
    (
        2 * CHUNK + 1,
        "db667b0f1c97872594e77fce56b41b26d310f09d5b2d71b0e71095cd41db3270",
    ),
];

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn golden_objects_are_unchanged_and_open() {
    let e = envelope();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let bless = std::env::var_os("KVIK_BLESS_GOLDEN").is_some();
    for (name, key, p) in golden_cases() {
        let sealed = e.seal(&key, &p).unwrap();
        let path = dir.join(name);
        if bless {
            fs::write(&path, &sealed).unwrap();
        }
        let file = fs::read(&path).unwrap();
        assert_eq!(file, sealed, "{name} differs from what sealing gives now");
        assert_eq!(e.open(&key, &file).unwrap(), p, "{name}");
    }
    let k = Keys::new(SID).probe();
    for (len, want) in GOLDEN_DIGESTS {
        let o = e.seal(&k, &plain(len, 7)).unwrap();
        assert_eq!(hex(&Sha256::digest(&o)), want, "{len}");
    }
}
