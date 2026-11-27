//! The gate cursor (field 4 of a `0x30` block) and the gate checkpoint (block `0x31`) of a cursor file
//! (`format.md` A.15; `spec.md` §6 Gates, Resume; Q-1391).

use crate::bytes::{i64_at, u16_at, u32_at, u64_at};
use crate::writer::Position;

use super::{pos_at, put_pos};

/// The block type of a gate checkpoint.
pub const CHECKPOINT_BLOCK: u8 = 0x31;
/// The length of a gate point (field 4, and each `set_by` of a checkpoint).
pub const GATE_POINT_LEN: usize = 32;
/// Most boundaries a levels gate has, so most pending holds an instance has.
const MAX_PENDING: u8 = 16;
/// The fixed part of a checkpoint body.
const HEAD_LEN: usize = 32;
/// The least bytes an instance record takes.
const INSTANCE_MIN: usize = 8 + GATE_POINT_LEN + 4 + 8 + 1;
/// The least bytes a series record takes.
const SERIES_MIN: usize = 5;

/// A gate transition's identity as a cursor file holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GateId {
    /// The cause entry's position; for a timer, the last position before `due`.
    pub at: Position,
    /// The timer's instant; `None` for a transition caused by an entry.
    pub due: Option<i64>,
    /// The gate.
    pub gate: u32,
    /// The instance's series.
    pub series: u32,
}

/// A consumer's gate cursor (field 4): everything at or before it is confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateCursor {
    /// Tag 0: nothing confirmed yet.
    Nothing,
    /// Tag 1: after this transition.
    Transition(GateId),
    /// Tag 2: after every transition at or below `at` and due at or before `now` (a confirmed resync).
    Frontier {
        /// Last position.
        at: Position,
        /// Commit-clock instant.
        now: i64,
    },
}

impl GateCursor {
    /// The highest position it names.
    pub fn position(&self) -> Option<Position> {
        match self {
            GateCursor::Nothing => None,
            GateCursor::Transition(id) => Some(id.at),
            GateCursor::Frontier { at, .. } => Some(*at),
        }
    }

    /// The latest commit-clock instant it names (Q-1394).
    pub fn instant(&self) -> Option<i64> {
        match self {
            GateCursor::Nothing => None,
            GateCursor::Transition(id) => id.due,
            GateCursor::Frontier { now, .. } => Some(*now),
        }
    }
}

/// A value as a checkpoint holds it.
#[derive(Clone, Debug, PartialEq)]
pub enum CheckpointValue {
    /// Tag 0: no value.
    Absent,
    /// Tag 1: a finite number.
    Num(f64),
    /// Tag 2: a boolean.
    Bool(bool),
    /// Tag 3: a named state or text, at most 255 bytes.
    Text(String),
}

/// An instance's state.
#[derive(Clone, Debug, PartialEq)]
pub enum CheckpointState {
    /// Tag 0.
    Unknown,
    /// Tag 1.
    Stale,
    /// Tag 2: boundaries crossed.
    Level(u8),
    /// Tag 3: in the set or not.
    Match(bool),
    /// Tag 4: the value delivered last.
    Value(CheckpointValue),
}

/// One gate instance at the checkpoint's frontier.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckpointInstance {
    /// Gate id.
    pub gate: u32,
    /// Series id.
    pub series: u32,
    /// State last reported.
    pub state: CheckpointState,
    /// The transition that set it.
    pub set_by: GateId,
    /// Boundaries crossed, a bit each.
    pub crossed: u16,
    /// Boundaries whose far side holds, by index, with the first instant it held (index order, at most 16).
    pub pending: Vec<(u8, i64)>,
    /// A change gate's reference value.
    pub reference: Option<CheckpointValue>,
    /// Commit-clock instant of the last evaluation.
    pub last_eval: i64,
}

/// A series' live-state fact as a checkpoint holds it.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckpointFact {
    /// Event time.
    pub t: i64,
    /// Position.
    pub position: Position,
    /// Value.
    pub value: CheckpointValue,
    /// Neither bad quality nor without a value.
    pub usable: bool,
    /// Revision of a versioned series.
    pub changed_at: Option<u64>,
}

/// One series an instance reads, at the checkpoint's frontier. Facts ahead of it are not held (read again at a
/// restore).
#[derive(Clone, Debug, PartialEq)]
pub struct CheckpointSeries {
    /// Series id.
    pub series: u32,
    /// Live-state fact.
    pub live: Option<CheckpointFact>,
    /// Latest arrival.
    pub last_arrival: Option<i64>,
}

/// A consumer's gates engine at a frontier (block `0x31`).
#[derive(Clone, Debug, PartialEq)]
pub struct Checkpoint {
    /// Last position applied.
    pub through: Position,
    /// Commit-clock instant reached.
    pub now: i64,
    /// Instances in `(gate, series)` order.
    pub instances: Vec<CheckpointInstance>,
    /// Series in id order.
    pub series: Vec<CheckpointSeries>,
}

impl Checkpoint {
    /// The highest position it names (a source of F, `spec.md` §3 Recovery rule 3).
    pub fn highest(&self) -> Position {
        self.instances
            .iter()
            .map(|i| i.set_by.at)
            .chain(
                self.series
                    .iter()
                    .filter_map(|s| s.live.as_ref().map(|l| l.position)),
            )
            .fold(self.through, Position::max)
    }
}

pub(super) fn put_point(out: &mut Vec<u8>, c: &GateCursor) {
    let (tag, at, due, gate, series) = match c {
        GateCursor::Nothing => (0u8, Position::default(), None, 0, 0),
        GateCursor::Transition(id) => (1, id.at, id.due, id.gate, id.series),
        GateCursor::Frontier { at, now } => (2, *at, Some(*now), 0, 0),
    };
    out.extend_from_slice(&[tag, u8::from(due.is_some()), 0, 0]);
    put_pos(out, at);
    out.extend_from_slice(&due.unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&gate.to_le_bytes());
    out.extend_from_slice(&series.to_le_bytes());
}

/// A gate point; `None` if it breaks a rule (unknown tag, presence byte, fields set that its tag leaves zero).
pub(super) fn point_at(v: &[u8]) -> Option<GateCursor> {
    if v.len() < GATE_POINT_LEN || v[2] != 0 || v[3] != 0 || v[1] > 1 {
        return None;
    }
    let at = pos_at(v, 4);
    let due = (v[1] == 1).then(|| i64_at(v, 16));
    let (gate, series) = (u32_at(v, 24), u32_at(v, 28));
    if v[1] == 0 && i64_at(v, 16) != 0 {
        return None;
    }
    match v[0] {
        0 if at == Position::default() && due.is_none() && gate == 0 && series == 0 => {
            Some(GateCursor::Nothing)
        }
        1 => Some(GateCursor::Transition(GateId {
            at,
            due,
            gate,
            series,
        })),
        2 if gate == 0 && series == 0 => due.map(|now| GateCursor::Frontier { at, now }),
        _ => None,
    }
}

fn put_value(out: &mut Vec<u8>, v: &CheckpointValue) -> Option<()> {
    match v {
        CheckpointValue::Absent => out.push(0),
        CheckpointValue::Num(x) if x.is_finite() => {
            out.push(1);
            out.extend_from_slice(&x.to_bits().to_le_bytes());
        }
        CheckpointValue::Num(_) => return None,
        CheckpointValue::Bool(b) => out.extend_from_slice(&[2, u8::from(*b)]),
        CheckpointValue::Text(s) => {
            out.push(3);
            out.push(u8::try_from(s.len()).ok()?);
            out.extend_from_slice(s.as_bytes());
        }
    }
    Some(())
}

/// A cursor over a checkpoint body; every read is checked against its end.
struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let v = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(v)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|v| v[0])
    }

    fn i64(&mut self) -> Option<i64> {
        self.take(8).map(|v| i64_at(v, 0))
    }

    fn value(&mut self) -> Option<CheckpointValue> {
        Some(match self.u8()? {
            0 => CheckpointValue::Absent,
            1 => {
                let x = f64::from_bits(u64_at(self.take(8)?, 0));
                if !x.is_finite() {
                    return None;
                }
                CheckpointValue::Num(x)
            }
            2 => match self.u8()? {
                b @ 0..=1 => CheckpointValue::Bool(b == 1),
                _ => return None,
            },
            3 => {
                let n = usize::from(self.u8()?);
                CheckpointValue::Text(std::str::from_utf8(self.take(n)?).ok()?.to_owned())
            }
            _ => return None,
        })
    }
}

/// A checkpoint's block body (A.15); `None` when a value is not representable (a non-finite number, text over 255
/// bytes, more than 16 pending holds).
pub fn encode_body(c: &Checkpoint) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(HEAD_LEN + c.instances.len() * 64 + c.series.len() * 48);
    put_pos(&mut out, c.through);
    out.extend_from_slice(&c.now.to_le_bytes());
    out.extend_from_slice(&u32::try_from(c.instances.len()).ok()?.to_le_bytes());
    out.extend_from_slice(&u32::try_from(c.series.len()).ok()?.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    for i in &c.instances {
        out.extend_from_slice(&i.gate.to_le_bytes());
        out.extend_from_slice(&i.series.to_le_bytes());
        put_point(&mut out, &GateCursor::Transition(i.set_by));
        let pending = u8::try_from(i.pending.len())
            .ok()
            .filter(|n| *n <= MAX_PENDING)?;
        let (tag, byte) = match &i.state {
            CheckpointState::Unknown => (0u8, 0u8),
            CheckpointState::Stale => (1, 0),
            CheckpointState::Level(n) => (2, *n),
            CheckpointState::Match(b) => (3, u8::from(*b)),
            CheckpointState::Value(_) => (4, 0),
        };
        out.extend_from_slice(&i.crossed.to_le_bytes());
        out.extend_from_slice(&[pending, tag]);
        out.extend_from_slice(&i.last_eval.to_le_bytes());
        for (k, since) in &i.pending {
            out.push(*k);
            out.extend_from_slice(&since.to_le_bytes());
        }
        match &i.state {
            CheckpointState::Level(_) | CheckpointState::Match(_) => out.push(byte),
            CheckpointState::Value(v) => put_value(&mut out, v)?,
            _ => {}
        }
        match &i.reference {
            None => out.push(0),
            Some(v) => {
                out.push(1);
                put_value(&mut out, v)?;
            }
        }
    }
    for s in &c.series {
        out.extend_from_slice(&s.series.to_le_bytes());
        let live = s.live.as_ref();
        let flags = u8::from(live.is_some())
            | u8::from(s.last_arrival.is_some()) << 1
            | u8::from(live.is_some_and(|l| l.usable)) << 2
            | u8::from(live.is_some_and(|l| l.changed_at.is_some())) << 3;
        out.push(flags);
        if let Some(a) = s.last_arrival {
            out.extend_from_slice(&a.to_le_bytes());
        }
        if let Some(l) = live {
            out.extend_from_slice(&l.t.to_le_bytes());
            put_pos(&mut out, l.position);
            if let Some(c) = l.changed_at {
                out.extend_from_slice(&c.to_le_bytes());
            }
            put_value(&mut out, &l.value)?;
        }
    }
    Some(out)
}

/// A checkpoint block body; `None` if it fails a check (counts beyond the body, order, a tag, trailing bytes).
pub fn decode_body(b: &[u8]) -> Option<Checkpoint> {
    if b.len() < HEAD_LEN || u32_at(b, 28) != 0 {
        return None;
    }
    let (n_inst, n_series) = (u32_at(b, 20) as usize, u32_at(b, 24) as usize);
    let room = b.len() - HEAD_LEN;
    if n_inst.checked_mul(INSTANCE_MIN)? > room
        || n_series.checked_mul(SERIES_MIN)? > room - n_inst * INSTANCE_MIN
    {
        return None;
    }
    let mut r = Rd { b, at: HEAD_LEN };
    let mut instances = Vec::with_capacity(n_inst);
    for _ in 0..n_inst {
        let head = r.take(8)?;
        let (gate, series) = (u32_at(head, 0), u32_at(head, 4));
        let GateCursor::Transition(set_by) = point_at(r.take(GATE_POINT_LEN)?)? else {
            return None;
        };
        let fixed = r.take(4)?;
        let (crossed, pending_n, tag) = (u16_at(fixed, 0), fixed[2], fixed[3]);
        if pending_n > MAX_PENDING {
            return None;
        }
        let last_eval = r.i64()?;
        let mut pending = Vec::with_capacity(usize::from(pending_n));
        for _ in 0..pending_n {
            pending.push((r.u8()?, r.i64()?));
        }
        let state = match tag {
            0 => CheckpointState::Unknown,
            1 => CheckpointState::Stale,
            2 => CheckpointState::Level(r.u8()?),
            3 => match r.u8()? {
                b @ 0..=1 => CheckpointState::Match(b == 1),
                _ => return None,
            },
            4 => CheckpointState::Value(r.value()?),
            _ => return None,
        };
        let reference = match r.u8()? {
            0 => None,
            1 => Some(r.value()?),
            _ => return None,
        };
        if instances
            .last()
            .is_some_and(|p: &CheckpointInstance| (p.gate, p.series) >= (gate, series))
        {
            return None;
        }
        instances.push(CheckpointInstance {
            gate,
            series,
            state,
            set_by,
            crossed,
            pending,
            reference,
            last_eval,
        });
    }
    let mut series = Vec::with_capacity(n_series);
    for _ in 0..n_series {
        let id = u32_at(r.take(4)?, 0);
        let flags = r.u8()?;
        if flags > 0b1111 || (flags & 1 == 0 && flags & 0b1100 != 0) {
            return None;
        }
        let last_arrival = if flags & 2 != 0 { Some(r.i64()?) } else { None };
        let live = if flags & 1 != 0 {
            let t = r.i64()?;
            let position = pos_at(r.take(12)?, 0);
            let changed_at = if flags & 8 != 0 {
                Some(u64_at(r.take(8)?, 0))
            } else {
                None
            };
            Some(CheckpointFact {
                t,
                position,
                value: r.value()?,
                usable: flags & 4 != 0,
                changed_at,
            })
        } else {
            None
        };
        if series
            .last()
            .is_some_and(|p: &CheckpointSeries| p.series >= id)
        {
            return None;
        }
        series.push(CheckpointSeries {
            series: id,
            live,
            last_arrival,
        });
    }
    if r.at != b.len() {
        return None;
    }
    Some(Checkpoint {
        through: pos_at(b, 0),
        now: i64_at(b, 12),
        instances,
        series,
    })
}
