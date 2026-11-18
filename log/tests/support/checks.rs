//! Helpers shared by the tests of the body checks (`format.md` A.5, A.6), batch assembly and the near-valid bodies of
//! `batches.rs`.

use kvik_log::BodyError;
use kvik_log::content::Item;
use kvik_log::entries::EntriesBlock;
use kvik_log::facts::{FactFlags, FactRow, FactsBlock, Quality, Value};

use super::*;

pub fn plain_columns(n: u32) -> Vec<(u8, u8, Vec<u8>)> {
    let n = n as usize;
    vec![
        (1, 0, vec![0; 4 * n]),
        (2, 0, vec![0; 8 * n]),
        (5, 0, vec![1; n]),
        (6, 0, vec![0; 8 * n]),
    ]
}

pub fn with(n: u32, f: impl FnOnce(&mut Vec<(u8, u8, Vec<u8>)>)) -> Result<FactsBlock, BodyError> {
    let mut c = plain_columns(n);
    f(&mut c);
    FactsBlock::decode(&facts_raw(n, &c))
}

pub fn insert(c: &mut Vec<(u8, u8, Vec<u8>)>, id: u8, enc: u8, payload: Vec<u8>) {
    let at = c.iter().position(|e| e.0 > id).unwrap_or(c.len());
    c.insert(at, (id, enc, payload));
}

pub type Fields = Vec<(u16, Vec<u8>)>;

pub fn decode_one(t: u8, fields: Fields) -> Result<EntriesBlock, BodyError> {
    EntriesBlock::decode(&entries_body(1, T0, &[(t, fields)]))
}

pub fn def_fields() -> Fields {
    definition_fields().1
}

pub fn definition_fields() -> Entry {
    definition(1, "room/temperature", "degC")
}

pub fn fact(v: Value) -> Item {
    Item::Fact(FactRow {
        series_id: 1,
        t: T0,
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: v,
        changed_at: None,
    })
}
