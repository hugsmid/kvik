//! The scan bound (`format.md` A, common rules; Q-110): the CRC shift and prefix table against direct checksums.

use kvik_log::crc::{PrefixCrc, crc32c, crc32c_shift};
use proptest::prelude::*;

proptest! {
    #[test]
    fn shifted_crc_of_a_prefix_combines_with_the_suffix_to_the_direct_crc(
        a in prop::collection::vec(any::<u8>(), 0..300),
        b in prop::collection::vec(any::<u8>(), 0..300),
    ) {
        let whole = crc32c(&[a.as_slice(), b.as_slice()].concat());
        prop_assert_eq!(crc32c_shift(crc32c(&a), b.len() as u64) ^ crc32c(&b), whole);
        prop_assert_eq!(whole ^ crc32c_shift(crc32c(&a), b.len() as u64), crc32c(&b));
    }

    #[test]
    fn a_prefix_table_range_equals_the_direct_crc(
        bytes in prop::collection::vec(any::<u8>(), 0..600),
        ranges in prop::collection::vec((0usize..80, 0usize..300), 1..12),
    ) {
        let mut t = PrefixCrc::new(&bytes);
        for (s, len) in ranges {
            let direct = bytes.get(8 * s..8 * s + len).map(crc32c);
            prop_assert_eq!(t.range((8 * s) as u64, len as u64), direct);
        }
    }
}
