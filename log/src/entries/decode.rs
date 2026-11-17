//! The decoder of entries bodies, with every check of `format.md` A.6.

use std::cell::RefCell;

use super::definition::{Names, definition};
use super::*;

/// The fields an entry type allows, the ones a parameter change forbids, and the repeated ones.
const fn allowed(t: u8, id: u16) -> Allowed {
    use Allowed::*;
    let ok = match t {
        entry_type::DEFINITION => matches!(id, 1..=19 | 22..=27),
        entry_type::PARAMETER_CHANGE => {
            if matches!(id, 2..=6 | 13..=19 | 22) {
                return Forbidden;
            }
            matches!(id, 1 | 7..=12 | 20 | 23..=27)
        }
        entry_type::ALIAS => matches!(id, 1 | 2),
        entry_type::RETIREMENT => matches!(id, 1 | 21),
        entry_type::ROUTING => matches!(id, 50..=58 | 60..=66),
        entry_type::SITE_ZONE => matches!(id, 20 | 30),
        entry_type::TZDATA => id == 31,
        entry_type::RECOVERY => matches!(id, 40..=47),
        entry_type::SCALE_RAISE => matches!(id, 1 | 5),
        _ => false,
    };
    match (ok, id) {
        (true, _) => Yes,
        (false, 0x8000..) => Ignorable,
        (false, _) => Unknown,
    }
}

enum Allowed {
    Yes,
    Forbidden,
    Ignorable,
    Unknown,
}

/// For each known entry type and each id below `IDS`: `ALLOWED` if the type allows it, and `REPEATED` if it repeats it.
const FLAGS: [[u8; IDS]; 10] = {
    let mut m = [[0u8; IDS]; 10];
    let mut t = 1;
    while t < 10 {
        let mut id = 0;
        while id < IDS {
            if let Allowed::Yes = allowed(t as u8, id as u16) {
                m[t][id] |= ALLOWED;
            }
            if repeated(t as u8, id as u16) {
                m[t][id] |= REPEATED;
            }
            id += 1;
        }
        t += 1;
    }
    m
};
const ALLOWED: u8 = 1;
const REPEATED: u8 = 2;

const fn repeated(t: u8, id: u16) -> bool {
    matches!(
        (t, id),
        (entry_type::DEFINITION, 13)
            | (entry_type::RECOVERY, 47)
            | (entry_type::ROUTING, 54)
            | (entry_type::ROUTING, 61)
    )
}

/// One past the largest id a known entry type allows.
const IDS: usize = 67;

/// The fields of one entry, framing and order checked. Without `keep`, text is checked but not copied (Q-651).
pub(super) struct Parsed<'a> {
    pub(super) entry: u32,
    fields: Vec<(u16, &'a [u8])>,
    /// Where the fields with each id lie in `fields`, which [`Parsed::split`] keeps in ascending order of id; only
    /// the ids of the present entry are set (Q-651).
    index: [(u32, u32); IDS],
    pub(super) keep: bool,
    pub(super) names: RefCell<Names>,
}

impl<'a> Parsed<'a> {
    fn new(keep: bool) -> Self {
        Parsed {
            entry: 0,
            fields: Vec::new(),
            index: [(0, 0); IDS],
            keep,
            names: RefCell::default(),
        }
    }
    pub(super) fn with(&self, id: u16) -> &[(u16, &'a [u8])] {
        let (from, to) = self.index.get(usize::from(id)).copied().unwrap_or_default();
        let run = &self.fields[from as usize..to as usize];
        debug_assert!(
            run.iter().all(|f| f.0 == id)
                && run.len() == self.fields.iter().filter(|f| f.0 == id).count(),
            "the index agrees with a linear search"
        );
        run
    }
    pub(super) fn get(&self, id: u16) -> Option<&'a [u8]> {
        self.with(id).first().map(|f| f.1)
    }
    pub(super) fn all(&self, id: u16) -> impl Iterator<Item = &'a [u8]> + '_ {
        self.with(id).iter().map(|f| f.1)
    }
    pub(super) fn bad(&self, field: u16) -> BodyError {
        ContentError::Field {
            entry: self.entry,
            field,
        }
        .into()
    }
    pub(super) fn presence(&self, field: u16) -> BodyError {
        ContentError::FieldPresence {
            entry: self.entry,
            field,
        }
        .into()
    }
    /// A field that must be carried.
    pub(super) fn need(&self, id: u16) -> Result<&'a [u8], BodyError> {
        self.get(id).ok_or(self.presence(id))
    }
    /// A field that must not be carried.
    pub(super) fn none(&self, id: u16) -> Result<(), BodyError> {
        if self.get(id).is_some() {
            Err(self.presence(id))
        } else {
            Ok(())
        }
    }
    pub(super) fn u8_in(&self, id: u16, max: u8) -> Result<u8, BodyError> {
        let v = self.need(id)?;
        if v.len() != 1 || v[0] > max {
            return Err(self.bad(id));
        }
        Ok(v[0])
    }
    pub(super) fn u32(&self, id: u16) -> Result<u32, BodyError> {
        let v = self.need(id)?;
        if v.len() != 4 {
            return Err(self.bad(id));
        }
        Ok(u32_at(v, 0))
    }
    pub(super) fn u64(&self, id: u16) -> Result<u64, BodyError> {
        let v = self.need(id)?;
        if v.len() != 8 {
            return Err(self.bad(id));
        }
        Ok(u64_at(v, 0))
    }
    pub(super) fn i64(&self, id: u16) -> Result<i64, BodyError> {
        self.u64(id).map(|v| v as i64)
    }
    /// An `i64` or none (zero length); a present value must be positive when `positive`.
    pub(super) fn opt_i64_of(
        &self,
        id: u16,
        v: &[u8],
        positive: bool,
    ) -> Result<Option<i64>, BodyError> {
        match v.len() {
            0 => Ok(None),
            8 if !positive || i64_at(v, 0) > 0 => Ok(Some(i64_at(v, 0))),
            _ => Err(self.bad(id)),
        }
    }
    pub(super) fn opt_u64_of(&self, id: u16, v: &[u8]) -> Result<Option<u64>, BodyError> {
        match v.len() {
            0 => Ok(None),
            8 => Ok(Some(u64_at(v, 0))),
            _ => Err(self.bad(id)),
        }
    }
    pub(super) fn text(
        &self,
        id: u16,
        v: &'a [u8],
        min: usize,
        max: usize,
    ) -> Result<&'a str, BodyError> {
        match std::str::from_utf8(v) {
            Ok(s) if (min..=max).contains(&s.len()) => Ok(s),
            _ => Err(self.bad(id)),
        }
    }
    pub(super) fn need_text(&self, id: u16, min: usize, max: usize) -> Result<&'a str, BodyError> {
        self.text(id, self.need(id)?, min, max)
    }
    /// The entry's copy of a checked text: empty without `keep`.
    pub(super) fn own(&self, s: &str) -> String {
        if self.keep {
            s.to_owned()
        } else {
            String::new()
        }
    }
}

pub(super) fn decode(body: &[u8]) -> Result<EntriesBlock, BodyError> {
    let mut sink = Collect(Vec::new());
    let p = walk(body, true, &mut sink)?;
    Ok(EntriesBlock {
        first_sequence: p.first_sequence,
        received_at: p.received_at,
        entries: sink.0,
    })
}

/// Applies every check of A.6 to a body without keeping its entries: its prefix, and whether it holds a recovery
/// entry. This is what a reader runs on every entries block it accepts; it copies no text (Q-651).
pub(crate) fn check(body: &[u8]) -> Result<(DataPrefix, bool), BodyError> {
    let mut sink = Check { recovery: false };
    let p = walk(body, false, &mut sink)?;
    Ok((p, sink.recovery))
}

/// Whether a body that passed [`check`] holds a recovery entry, read from the entry headers alone.
pub(crate) fn holds_recovery(body: &[u8]) -> bool {
    let Ok(p) = DataPrefix::decode(block_type::ENTRIES, body) else {
        return false;
    };
    let mut at = DATA_PREFIX_LEN;
    for _ in 0..p.count {
        let Some(head) = body.get(at..at + 8) else {
            return false;
        };
        if head[0] == entry_type::RECOVERY {
            return true;
        }
        at = at
            .saturating_add(8)
            .saturating_add(u32_at(head, 4) as usize)
            .div_ceil(8)
            .saturating_mul(8);
    }
    false
}

/// What a walk does with each entry it has read.
trait Sink {
    fn entry(&mut self, t: u8, p: &Parsed<'_>) -> Result<(), BodyError>;
    fn ignored(&mut self);
}

struct Collect(Vec<Entry>);

impl Sink for Collect {
    fn entry(&mut self, t: u8, p: &Parsed<'_>) -> Result<(), BodyError> {
        self.0.push(entry(t, p)?);
        Ok(())
    }
    fn ignored(&mut self) {
        // Consecutive ignored entries are counted, not stored one by one.
        match self.0.last_mut() {
            Some(Entry::Ignored { count }) => *count += 1,
            _ => self.0.push(Entry::Ignored { count: 1 }),
        }
    }
}

/// The verdict alone, building no entry that needs memory (Q-651).
struct Check {
    recovery: bool,
}

impl Sink for Check {
    fn entry(&mut self, t: u8, p: &Parsed<'_>) -> Result<(), BodyError> {
        match t {
            entry_type::DEFINITION => definition(p).map(drop),
            entry_type::ROUTING => routing(p).map(drop),
            _ => {
                self.recovery |= t == entry_type::RECOVERY;
                entry(t, p).map(drop)
            }
        }
    }
    fn ignored(&mut self) {}
}

/// Reads the entries of a body one at a time, handing each to `sink`; without `keep` their text is left empty.
fn walk(body: &[u8], keep: bool, sink: &mut impl Sink) -> Result<DataPrefix, BodyError> {
    let p = DataPrefix::decode(block_type::ENTRIES, body).map_err(|_| ContentError::Prefix)?;
    let mut at = DATA_PREFIX_LEN;
    let mut parsed = Parsed::new(keep);
    for i in 0..p.count {
        let overrun = BodyError::from(ContentError::EntryOverrun { entry: i });
        let head = body.get(at..at + 8).ok_or(overrun)?;
        let (t, len) = (head[0], u32_at(head, 4) as usize);
        let fields_at = at + 8;
        let raw = fields_at
            .checked_add(len)
            .and_then(|end| body.get(fields_at..end))
            .ok_or(overrun)?;
        at = (fields_at + len).div_ceil(8) * 8;
        parsed.entry = i;
        let known = matches!(t, 1..=9);
        if !known {
            if t >= 0x80 {
                parsed.split(raw, t, false)?;
                sink.ignored();
                continue;
            }
            return Err(BodyError::Unsupported(Element::EntryType(t)));
        }
        parsed.split(raw, t, true)?;
        sink.entry(t, &parsed)?;
    }
    // Bytes after the last entry are not checked here (Q-106 (a)); the canonical decoder rejects them.
    Ok(p)
}

impl<'a> Parsed<'a> {
    /// Splits a field list, checking framing and, for a known type, order and allowed ids.
    fn split(&mut self, raw: &'a [u8], t: u8, known: bool) -> Result<(), BodyError> {
        for &(id, _) in &self.fields {
            self.index[usize::from(id)] = (0, 0);
        }
        self.fields.clear();
        let entry = self.entry;
        let flags: &[u8] = FLAGS.get(usize::from(t)).map_or(&[], |f| f);
        let mut at = 0;
        // The largest id so far, `-1` before the first.
        let mut prev = -1i32;
        while at < raw.len() {
            let h = raw.get(at..at + 4).ok_or_else(|| overrun(entry))?;
            let (id, len) = (u16_at(h, 0), usize::from(u16_at(h, 2)));
            let v = raw
                .get(at + 4..at + 4 + len)
                .ok_or_else(|| overrun(entry))?;
            // The field's padding to 4 is part of the entry's `length`.
            at = (at + 4 + len).div_ceil(4) * 4;
            if at > raw.len() {
                return Err(overrun(entry));
            }
            if !known {
                continue;
            }
            let f = flags.get(usize::from(id)).copied().unwrap_or(0);
            let allowed_here = f & ALLOWED != 0;
            // A version refusal wins over invalid content: unknown ids are classified before order is tested.
            if !allowed_here && id < 0x8000 {
                return Err(match allowed(t, id) {
                    Allowed::Forbidden => ContentError::FieldNotAllowed { entry, field: id }.into(),
                    _ => BodyError::Unsupported(Element::Field {
                        entry_type: t,
                        field: id,
                    }),
                });
            }
            let id32 = i32::from(id);
            if id < 0x8000 && (id32 < prev || (id32 == prev && f & REPEATED == 0)) {
                return Err(ContentError::FieldOrder { entry, field: id }.into());
            }
            prev = prev.max(id32);
            if allowed_here {
                // An allowed id is below `IDS` and comes in ascending order, so its fields are one run.
                let n = self.fields.len() as u32;
                let slot = &mut self.index[usize::from(id)];
                if slot.1 == 0 {
                    slot.0 = n;
                }
                slot.1 = n + 1;
                self.fields.push((id, v));
            }
        }
        Ok(())
    }
}

#[cold]
fn overrun(entry: u32) -> BodyError {
    ContentError::EntryOverrun { entry }.into()
}

fn entry(t: u8, p: &Parsed<'_>) -> Result<Entry, BodyError> {
    Ok(match t {
        entry_type::DEFINITION => Entry::Definition(Box::new(definition(p)?)),
        entry_type::PARAMETER_CHANGE => Entry::ParameterChange(parameter_change(p)?),
        entry_type::ALIAS => Entry::Alias {
            id: p.u32(1)?,
            name: p.own(p.need_text(2, 1, 255)?),
        },
        entry_type::RETIREMENT => Entry::Retirement {
            id: p.u32(1)?,
            t_r: p.i64(21)?,
        },
        entry_type::ROUTING => Entry::Routing(Box::new(routing(p)?)),
        entry_type::SITE_ZONE => Entry::SiteZone {
            zone: p.own(p.need_text(30, 1, 63)?),
            effective: p.i64(20)?,
        },
        entry_type::TZDATA => {
            let v = p.need_text(31, 1, 15)?;
            if !v.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(p.bad(31));
            }
            Entry::Tzdata { version: p.own(v) }
        }
        entry_type::RECOVERY => {
            let boot = p.need(42)?;
            let boot_id: [u8; 16] = boot.try_into().map_err(|_| p.bad(42))?;
            let mark_state = match p.u8_in(45, 3)? {
                0 => MarkState::Readable,
                1 => MarkState::Missing,
                2 => MarkState::Unreadable,
                _ => MarkState::Stale,
            };
            let stamp = match p.get(41) {
                None => None,
                Some(v) => match ResetStamp::decode(v) {
                    Some(st) if st.nonce != 0 => Some(st),
                    _ => return Err(p.bad(41)),
                },
            };
            let mut consumers = Vec::new();
            for v in p.all(47) {
                match std::str::from_utf8(v) {
                    Ok(s) if consumer_name_ok(s) => {
                        if p.keep {
                            consumers.push(s.to_owned());
                        }
                    }
                    _ => return Err(p.bad(47)),
                }
            }
            Entry::Recovery(Recovery {
                after: p.u64(40)?,
                stamp,
                boot_id,
                tail_bytes: p.u64(43)?,
                readable_commits: p.u32(44)?,
                mark_state,
                id_floor: p.u32(46)?,
                consumers,
            })
        }
        _ => Entry::ScaleRaise {
            id: p.u32(1)?,
            scale: p.u8_in(5, MAX_SCALE)?,
        },
    })
}

/// A selection whose kind is field `kind` (53 or 60); ids for a pattern only when `resolved_ok`.
fn read_selection(p: &Parsed<'_>, kind: u16, resolved_ok: bool) -> Result<Selection, BodyError> {
    let mut ids = Vec::new();
    let mut any = false;
    for v in p.all(kind + 1) {
        if v.len() != 4 {
            return Err(p.bad(kind + 1));
        }
        any = true;
        if p.keep {
            ids.push(u32_at(v, 0));
        }
    }
    Ok(match p.u8_in(kind, 3)? {
        0 | 3 => {
            if any {
                return Err(p.presence(kind + 1));
            }
            p.none(kind + 2)?;
            if p.u8_in(kind, 3)? == 0 {
                Selection::None
            } else {
                Selection::All
            }
        }
        1 => {
            p.none(kind + 2)?;
            Selection::Ids(ids)
        }
        _ => {
            if !resolved_ok && any {
                return Err(p.presence(kind + 1));
            }
            Selection::Pattern {
                pattern: p.own(p.need_text(kind + 2, 1, 255)?),
                resolved: ids,
            }
        }
    })
}

fn finite(p: &Parsed<'_>, id: u16) -> Result<f64, BodyError> {
    let v = f64::from_bits(p.u64(id)?);
    if v.is_finite() { Ok(v) } else { Err(p.bad(id)) }
}

fn non_negative(p: &Parsed<'_>, id: u16) -> Result<i64, BodyError> {
    let v = p.i64(id)?;
    if v >= 0 { Ok(v) } else { Err(p.bad(id)) }
}

fn routing(p: &Parsed<'_>) -> Result<Routing, BodyError> {
    let v = p.need(50)?;
    let consumer = match std::str::from_utf8(v) {
        Ok(s) if consumer_name_ok(s) => p.own(s),
        _ => return Err(p.bad(50)),
    };
    let op = match p.u8_in(51, 3)? {
        1 => RouteOp::Add,
        2 => RouteOp::Remove,
        3 => RouteOp::Emits,
        _ => return Err(p.bad(51)),
    };
    let funnel_fields = [53, 54, 55, 56, 57, 58, 63, 64, 65, 66];
    let funnel = match op {
        RouteOp::Emits => p.none(52).map(|()| None)?,
        _ => Some(p.u32(52)?),
    };
    let (selection, operator, condition) = if op == RouteOp::Add {
        let selection = read_selection(p, 53, true)?;
        let operator = match p.get(56) {
            None => {
                p.none(57)?;
                p.none(58)?;
                None
            }
            Some(_) => {
                let aggregate = p.u8_in(56, 10)?;
                if aggregate == 0 {
                    return Err(p.bad(56));
                }
                let width = p.i64(57)?;
                if width <= 0 {
                    return Err(p.bad(57));
                }
                let state = if aggregate == 10 {
                    let v = p.need(58)?;
                    match (v.len(), v.first()) {
                        (5, Some(0)) if v[1] <= 1 && v[2..].iter().all(|b| *b == 0) => {
                            Some(StateArg::Bool(v[1] == 1))
                        }
                        (5, Some(1)) => Some(StateArg::State(u32_at(v, 1))),
                        _ => return Err(p.bad(58)),
                    }
                } else {
                    p.none(58).map(|()| None)?
                };
                Some(RouteOperator {
                    aggregate,
                    width,
                    state,
                })
            }
        };
        let condition = match p.get(63) {
            None => {
                for id in [64, 65, 66] {
                    p.none(id)?;
                }
                None
            }
            Some(_) => Some(match p.u8_in(63, 4)? {
                1 => {
                    p.none(66)?;
                    let (on, clear) = (finite(p, 64)?, finite(p, 65)?);
                    if clear > on {
                        return Err(p.bad(65));
                    }
                    RouteCondition::Threshold { on, clear }
                }
                2 => {
                    p.none(65)?;
                    p.none(66)?;
                    let deadband = finite(p, 64)?;
                    if deadband < 0.0 {
                        return Err(p.bad(64));
                    }
                    RouteCondition::OnChange { deadband }
                }
                3 => {
                    p.none(65)?;
                    RouteCondition::HeldFor {
                        above: finite(p, 64)?,
                        n: non_negative(p, 66)?,
                    }
                }
                4 => {
                    p.none(64)?;
                    p.none(65)?;
                    RouteCondition::StaleFor {
                        n: non_negative(p, 66)?,
                    }
                }
                _ => return Err(p.bad(63)),
            }),
        };
        (Some(selection), operator, condition)
    } else {
        for id in funnel_fields {
            p.none(id)?;
        }
        (None, None, None)
    };
    Ok(Routing {
        consumer,
        op,
        funnel,
        selection,
        operator,
        emits: read_selection(p, 60, false)?,
        condition,
    })
}

/// A consumer name: `[a-z0-9][a-z0-9._-]{0,63}` (A.12).
pub fn consumer_name_ok(s: &str) -> bool {
    let b = s.as_bytes();
    let first = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty()
        && b.len() <= 64
        && first(b[0])
        && b[1..]
            .iter()
            .all(|&c| first(c) || matches!(c, b'.' | b'_' | b'-'))
}

fn parameter_change(p: &Parsed<'_>) -> Result<ParameterChange, BodyError> {
    let opt = |id| p.get(id);
    let expected_interval = opt(7).map(|v| p.opt_i64_of(7, v, true)).transpose()?;
    let staleness = match (opt(8), opt(27)) {
        (Some(v), Some(_)) => Some((p.opt_i64_of(8, v, true)?, p.u8_in(27, 1)? == 1)),
        (None, None) => None,
        (Some(_), None) => return Err(p.presence(27)),
        (None, Some(_)) => return Err(p.presence(8)),
    };
    // A change of 7 is written with the staleness limit in force after it.
    if expected_interval.is_some() && staleness.is_none() {
        return Err(p.presence(8));
    }
    let change = ParameterChange {
        id: p.u32(1)?,
        effective: opt(20).map(|_| p.i64(20)).transpose()?,
        expected_interval,
        staleness,
        record_on_change: opt(9).map(|_| p.u8_in(9, 1).map(|v| v == 1)).transpose()?,
        wrap_at: opt(10).map(|v| p.opt_u64_of(10, v)).transpose()?,
        reset_ceiling_ppm: opt(11).map(|_| p.u32(11)).transpose()?,
        lateness_bound: opt(12).map(|_| p.i64(12)).transpose()?,
        retention_class: opt(23).map(|_| p.u8_in(23, 1)).transpose()?,
        retention_period: opt(24).map(|_| p.i64(24)).transpose()?,
        excess_precision: match opt(25) {
            None => p.none(26).map(|_| None)?,
            Some(_) => Some(match p.u8_in(25, 2)? {
                0 => p.none(26).map(|_| ExcessPrecision::Round)?,
                1 => p.none(26).map(|_| ExcessPrecision::Refuse)?,
                _ => ExcessPrecision::AutoRaise {
                    max_scale: p.u8_in(26, MAX_SCALE)?,
                },
            }),
        },
    };
    let timed = [7, 8, 9, 10, 11, 12].iter().any(|&id| opt(id).is_some());
    if timed != change.effective.is_some() {
        return Err(p.presence(20));
    }
    Ok(change)
}
