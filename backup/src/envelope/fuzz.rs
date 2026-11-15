//! The entry point of the fuzz target (`backup/fuzz`), also run over generated inputs by the stable test suite.

use super::{C, ChunkedXChaCha, Envelope, HEADER, MAGIC};
use kvik_log::crc::crc32c;

/// The key the fuzz inputs are opened under.
pub const KEY: [u8; 32] = [0x5a; 32];
/// The store id the fuzz inputs are opened under.
pub const STORE_ID: [u8; 16] = [0xa5; 16];
/// The object key the fuzz inputs are opened under.
pub const OBJECT_KEY: &str = "a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5/log/00000000-0000000000000001.kvl";

/// Opens `data` whole and in ranges, as it is and with a header made valid for [`KEY`], and seals and opens `data`
/// as a plaintext. Panics only if a check fails (a bug); never for any input. Every allocation is bounded by
/// `data.len()`, never by a length the bytes declare.
pub fn open_all(data: &[u8]) {
    let e = ChunkedXChaCha::new(KEY, STORE_ID);
    open_checked(&e, data);
    if data.len() >= HEADER {
        let mut fixed = data.to_vec();
        fixed[..4].copy_from_slice(&MAGIC);
        fixed[4] = 1;
        fixed[5] = 1;
        fixed[6..8].fill(0);
        fixed[16..24].copy_from_slice(&e.key_check().0);
        fixed[43..60].fill(0);
        let crc = crc32c(&fixed[..60]);
        fixed[60..64].copy_from_slice(&crc.to_le_bytes());
        e.check_header(&fixed).expect("a repaired header passes");
        open_checked(&e, &fixed);
    }
    let sealed = e.seal(OBJECT_KEY, data).expect("any slice in memory fits");
    assert_eq!(e.open(OBJECT_KEY, &sealed).as_deref(), Ok(data));
    assert_eq!(e.seal(OBJECT_KEY, data).as_ref(), Ok(&sealed));
}

fn open_checked(e: &ChunkedXChaCha, data: &[u8]) {
    let whole = e.open(OBJECT_KEY, data);
    if let Ok(plain) = &whole {
        assert_eq!(
            e.seal(OBJECT_KEY, plain).as_deref(),
            Ok(data),
            "an object that opens is the one sealing gives"
        );
    }
    let Ok(()) = e.check_header(data) else {
        return;
    };
    let len = u64::from_le_bytes(data[8..16].try_into().expect("8 bytes"));
    let seed = data.get(HEADER).copied().unwrap_or(0) as u64;
    let ranges = [
        0..len,
        0..len.min(1),
        len.saturating_sub(1)..len,
        (seed * 997).min(len)..(seed * 997 + C + 1).min(len),
        len.saturating_sub(C + 7)..len.saturating_sub(3),
    ];
    let object_len = data.len() as u64;
    for r in ranges {
        let span = e.span(r.clone());
        let lo = usize::try_from(span.start.min(object_len)).expect("within data");
        let hi = usize::try_from(span.end.min(object_len)).expect("within data");
        let part = e.open_span(OBJECT_KEY, &data[..HEADER], r.clone(), &data[lo..hi]);
        if let Ok(plain) = &whole {
            let (a, b) = (r.start as usize, r.end as usize);
            assert_eq!(part.as_deref(), Ok(&plain[a..b]), "a range is the slice");
        }
    }
}
