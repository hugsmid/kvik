//! Property tests of `docs/format-chunk.md`: every valid block round-trips and its encoding is canonical; every
//! column codec round-trips any values; damaged bodies are rejected or decode to content that is valid in turn;
//! truncation is always rejected.

mod support;

use kvik_chunk::float::{
    best_decimal, decode_decimal, decode_xor, decode_xor_zstd, encode_decimal, encode_xor,
    encode_xor_zstd, max_xor_len, min_exponent, scaled,
};
use kvik_chunk::fuzz::equivalent_canonical;
use kvik_chunk::ints::{
    decode_delta, decode_plain, decode_run_length, delta_len, encode_delta, encode_plain,
    encode_run_length,
};
use kvik_chunk::{Block, DecodeError, encoding};
use proptest::prelude::*;
use support::*;

/// Every value column a writer could consider for `b`: encoding and payload.
fn value_alternatives(b: &Block) -> Vec<(u8, Vec<u8>)> {
    let words = value_words(b);
    let ints: Vec<i64> = words.iter().map(|&w| w as i64).collect();
    let mut payloads = vec![
        (encoding::PLAIN, encode_plain(&words)),
        (encoding::RUN_LENGTH, encode_run_length(&words)),
        (encoding::DELTA, encode_delta(&ints)),
    ];
    payloads.extend((0..=18).map(|e| (encoding::DECIMAL, encode_decimal(&words, e))));
    let xor = encode_xor(&words);
    for level in [1, 3, 19] {
        let mut p = (xor.len() as u32).to_le_bytes().to_vec();
        p.extend_from_slice(&zstd::bulk::compress(&xor, level).unwrap());
        payloads.push((encoding::XOR_ZSTD, p));
    }
    payloads.push((encoding::XOR, xor));
    payloads
}

/// Whether C.10 makes the value column `(enc, payload)` canonical for `b`, from uncompressed lengths only (C.9).
/// `payload` in encoding 5 is a frame of the canonical XOR stream.
fn canonical_value(b: &Block, enc: u8, payload: &[u8]) -> bool {
    let words = value_words(b);
    let all_f64 = !words.is_empty()
        && (0..b.len())
            .filter(|&i| b.flags[i] & kvik_chunk::flag::NO_VALUE == 0)
            .all(|i| b.value_type[i] == kvik_chunk::value_type::F64);
    let ints: Vec<i64> = words.iter().map(|&w| w as i64).collect();
    let mut best = (encoding::PLAIN, encode_plain(&words));
    let mut offer = |e: u8, p: Vec<u8>| {
        if p.len() < best.1.len() {
            best = (e, p);
        }
    };
    offer(encoding::RUN_LENGTH, encode_run_length(&words));
    offer(encoding::DELTA, encode_delta(&ints));
    let decimal = all_f64.then(|| best_decimal(&words)).flatten();
    if let Some((e, _, _)) = decimal {
        offer(encoding::DECIMAL, encode_decimal(&words, e));
    }
    let class = all_f64 && decimal.is_none_or(|(_, k, _)| k * 8 > words.len());
    let xor = encode_xor(&words);
    if class && xor.len() < best.1.len() {
        (enc == encoding::XOR && payload == xor.as_slice())
            || (enc == encoding::XOR_ZSTD && payload.len() < xor.len())
    } else {
        (enc, payload) == (best.0, best.1.as_slice())
    }
}

/// Checks what any body that decodes must satisfy.
fn check_decoded(body: &[u8]) {
    if let Ok(b) = Block::decode(body) {
        let again = b.encode().expect("decoded content is valid");
        assert_eq!(Block::decode(&again), Ok(b.clone()));
        if Block::decode_canonical(body).is_ok() {
            assert!(equivalent_canonical(body, &again));
            assert_eq!(Block::decode_canonical(&again), Ok(b));
        }
    }
}

proptest! {
    #[test]
    fn block_round_trip_and_canonical(b in any_block()) {
        let body = b.encode().unwrap();
        prop_assert_eq!(Block::decode(&body), Ok(b.clone()));
        prop_assert_eq!(Block::decode_canonical(&body), Ok(b.clone()));
        prop_assert_eq!(Block::decode(&body).unwrap().encode().unwrap(), body);
    }

    #[test]
    fn exactly_the_canonical_value_columns_are_accepted(b in prop_oneof![any_block(), xor_block()]) {
        let body = b.encode().unwrap();
        for (enc, payload) in value_alternatives(&b) {
            let alt = with_value(&body, enc, &payload);
            let got = Block::decode_canonical(&alt);
            prop_assert_eq!(got.is_ok(), canonical_value(&b, enc, &payload), "encoding {}", enc);
            if let Ok(d) = got {
                prop_assert_eq!(&d, &b);
                prop_assert!(equivalent_canonical(&alt, &body));
            }
        }
    }

    #[test]
    fn truncation_is_rejected(b in any_block(), cut in any::<prop::sample::Index>()) {
        let body = b.encode().unwrap();
        let k = cut.index(body.len());
        prop_assert!(Block::decode(&body[..k]).is_err());
        let mut long = body.clone();
        long.push(0);
        prop_assert_eq!(Block::decode(&long), Err(DecodeError::BodyLength));
    }

    #[test]
    fn damaged_bodies_are_rejected_or_valid(
        b in any_block(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..6),
    ) {
        let mut body = b.encode().unwrap();
        for (at, byte) in edits {
            let i = at.index(body.len());
            body[i] ^= byte | 1;
        }
        check_decoded(&body);
    }

    #[test]
    fn random_bodies_never_panic(data in prop::collection::vec(any::<u8>(), 0..512)) {
        check_decoded(&data);
    }

    #[test]
    fn delta_round_trip(xs in prop::collection::vec(any::<i64>(), 0..600)) {
        let e = encode_delta(&xs);
        prop_assert_eq!(e.len(), delta_len(&xs));
        prop_assert_eq!(decode_delta(&e, xs.len()), Ok(xs.clone()));
    }

    #[test]
    fn delta_round_trip_on_grids(start in any::<i64>(), step in any::<i64>(), jitter in prop::collection::vec(0i64..1_000, 1..600)) {
        let xs: Vec<i64> = jitter.iter().enumerate().map(|(i, j)| start.wrapping_add(step.wrapping_mul(i as i64)).wrapping_add(*j)).collect();
        prop_assert_eq!(decode_delta(&encode_delta(&xs), xs.len()), Ok(xs));
    }

    #[test]
    fn run_length_round_trip(xs in prop::collection::vec(prop_oneof![Just(0u64), Just(7), any::<u64>()], 0..600)) {
        let e = encode_run_length(&xs);
        prop_assert_eq!(decode_run_length::<u64>(&e, xs.len()), Ok(xs.clone()));
        let p = encode_plain(&xs);
        prop_assert_eq!(decode_plain::<u64>(&p, xs.len()), Ok(xs));
    }

    #[test]
    fn decimal_round_trip_at_any_exponent(vs in prop::collection::vec(f64_bits(), 0..600), e in 0u8..=18) {
        let enc = encode_decimal(&vs, e);
        prop_assert_eq!(decode_decimal(&enc, vs.len()), Ok(vs.clone()));
        // Canonical within the encoding: the decoded values encode to the same bytes.
        prop_assert_eq!(encode_decimal(&decode_decimal(&enc, vs.len()).unwrap(), e), enc);
    }

    #[test]
    fn best_decimal_is_the_shortest_candidate(vs in prop::collection::vec(f64_bits(), 1..300)) {
        let mut candidates: Vec<u8> = vs.iter().filter_map(|&b| min_exponent(b)).collect();
        candidates.sort_unstable();
        candidates.dedup();
        match best_decimal(&vs) {
            None => prop_assert!(candidates.is_empty()),
            Some((e, _, len)) => {
                prop_assert_eq!(encode_decimal(&vs, e).len(), len);
                for c in candidates {
                    let l = encode_decimal(&vs, c).len();
                    prop_assert!(l > len || (l == len && c >= e));
                }
            }
        }
    }

    #[test]
    fn integers_below_2_pow_53_fit_at_exponent_0(n in -9_007_199_254_740_991i64..=9_007_199_254_740_991) {
        prop_assert_eq!(scaled((n as f64).to_bits(), 0), Some(n));
    }

    #[test]
    fn scaled_values_are_exact(bits in f64_bits(), e in 0u8..=18) {
        if let Some(m) = scaled(bits, e) {
            prop_assert!(m.unsigned_abs() < 1 << 53);
            prop_assert_eq!((m as f64 / 10f64.powi(i32::from(e))).to_bits(), bits);
        }
    }

    #[test]
    fn xor_round_trip(vs in prop::collection::vec(f64_bits(), 0..600)) {
        let enc = encode_xor(&vs);
        prop_assert!(enc.len() <= max_xor_len(vs.len()));
        prop_assert_eq!(decode_xor(&enc, vs.len()), Ok(vs.clone()));
        prop_assert_eq!(encode_xor(&decode_xor(&enc, vs.len()).unwrap()), enc.clone());
        let z = encode_xor_zstd(&enc).unwrap();
        prop_assert_eq!(decode_xor_zstd(&z, vs.len()), Ok(vs));
    }

    #[test]
    fn column_decoders_reject_or_round_trip(
        data in prop::collection::vec(any::<u8>(), 0..300),
        n in 0usize..200,
        e in 0u8..=18,
    ) {
        if let Ok(v) = decode_delta(&data, n) {
            prop_assert_eq!(v.len(), n);
            prop_assert_eq!(decode_delta(&encode_delta(&v), n), Ok(v));
        }
        if let Ok(v) = decode_run_length::<u16>(&data, n) {
            prop_assert_eq!(encode_run_length(&v), data.clone());
        }
        if let Ok(v) = decode_xor(&data, n) {
            prop_assert_eq!(decode_xor(&encode_xor(&v), n), Ok(v));
        }
        let mut dec = vec![e];
        dec.extend_from_slice(&data);
        if let Ok(v) = decode_decimal(&dec, n) {
            prop_assert_eq!(decode_decimal(&encode_decimal(&v, e), n), Ok(v));
        }
    }
}

#[test]
fn value_encodings_are_chosen_as_specified() {
    let enc_of = |b: &Block| {
        let body = b.encode().unwrap();
        // Directory entries follow the 3-byte prefix; the value column is the first with id 6.
        let mut at = 3;
        loop {
            let (id, enc) = (body[at], body[at + 1]);
            at += 2;
            while body[at] & 0x80 != 0 {
                at += 1;
            }
            at += 1;
            if id == 6 {
                return enc;
            }
        }
    };
    assert_eq!(enc_of(&temperature(4096)), encoding::DECIMAL);
    let mut ninths = temperature(4096);
    for (i, v) in ninths.value.iter_mut().enumerate() {
        *v = (f64::from((i % 5) as u32) / 9.0 + 1.0).to_bits();
    }
    assert_eq!(enc_of(&ninths), encoding::DELTA);
    let mut pairs = temperature(4096);
    for (i, v) in pairs.value.iter_mut().enumerate() {
        *v = (f64::from((i / 2 % 5) as u32) / 9.0 + 1.0).to_bits();
    }
    assert_eq!(enc_of(&pairs), encoding::XOR_ZSTD);
    let mut random = temperature(40);
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    for v in random.value.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        // Random signs and 20 random mantissa bits: not decimal; XOR drops the 32 trailing zero bits, while the
        // sign changes make the deltas 64 bits wide and the random bits leave zstd nothing to find.
        let m = 1.0 + (x % (1 << 20)) as f64 / f64::from(1u32 << 20);
        *v = (if x >> 63 == 1 { -m } else { m }).to_bits();
    }
    assert_eq!(enc_of(&random), encoding::XOR);
    let mut walk = temperature(512);
    walk.value_type = vec![2; 512];
    walk.scale = vec![2; 512];
    for (i, v) in walk.value.iter_mut().enumerate() {
        *v = (2_000 + (i as i64 % 17)) as u64;
    }
    assert_eq!(enc_of(&walk), encoding::DELTA);
    let mut states = temperature(512);
    states.value_type = vec![3; 512];
    for (i, v) in states.value.iter_mut().enumerate() {
        *v = (i / 100) as u64 % 2;
    }
    assert_eq!(enc_of(&states), encoding::RUN_LENGTH);
}
