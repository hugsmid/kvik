//! Log segment file names (`format.md` A.12): `<generation:8 hex>-<file_id:16 hex>.open` or `.kvl`.

use std::fmt;

/// The name of a log segment file under `log/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentName {
    /// The generation in which the file was created.
    pub generation: u32,
    /// The file id.
    pub file_id: u64,
    /// `.kvl` (sealed) rather than `.open`.
    pub sealed: bool,
}

impl SegmentName {
    /// Parses a name exactly: lower-case hex, zero-padded to the stated widths, a lower-case extension. Any other
    /// name is not a segment and is ignored by readers.
    pub fn parse(name: &str) -> Option<Self> {
        let (stem, sealed) = if let Some(s) = name.strip_suffix(".open") {
            (s, false)
        } else {
            (name.strip_suffix(".kvl")?, true)
        };
        let (g, f) = stem.split_once('-')?;
        Some(Self {
            generation: hex(g, 8)? as u32,
            file_id: hex(f, 16)?,
            sealed,
        })
    }
}

fn hex(s: &str, width: usize) -> Option<u64> {
    let ok = s.len() == width
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    ok.then(|| u64::from_str_radix(s, 16).ok()).flatten()
}

impl fmt::Display for SegmentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ext = if self.sealed { "kvl" } else { "open" };
        write!(f, "{:08x}-{:016x}.{ext}", self.generation, self.file_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_names_only() {
        let n = SegmentName {
            generation: 2,
            file_id: 0xab,
            sealed: false,
        };
        assert_eq!(n.to_string(), "00000002-00000000000000ab.open");
        assert_eq!(
            SegmentName::parse("00000002-00000000000000ab.open"),
            Some(n)
        );
        assert_eq!(
            SegmentName::parse("ffffffff-ffffffffffffffff.kvl"),
            Some(SegmentName {
                generation: u32::MAX,
                file_id: u64::MAX,
                sealed: true
            })
        );
        for bad in [
            "00000002-00000000000000AB.open",
            "00000002-00000000000000ab.OPEN",
            "2-ab.open",
            "00000002-00000000000000ab.kvl.tmp",
            "00000002_00000000000000ab.open",
            "+0000002-00000000000000ab.open",
            "00000002-+000000000000000ab.open",
        ] {
            assert_eq!(SegmentName::parse(bad), None, "{bad}");
        }
    }
}
