//! Series definitions (`format.md` A.6, entry type 1).

use super::decode::Parsed;
use super::*;

pub(super) fn definition(p: &Parsed<'_>) -> Result<Definition, BodyError> {
    let kind = match p.u8_in(3, 5)? {
        1 => Kind::Gauge,
        2 => Kind::Counter,
        3 => Kind::State,
        4 => Kind::Text,
        5 => Kind::Interval,
        _ => return Err(p.bad(3)),
    };
    let value_type = ValueType::from_code(p.u8_in(4, 5)?).ok_or(p.bad(4))?;
    let decimal = value_type == ValueType::Decimal;
    let scale = if decimal {
        Some(p.u8_in(5, MAX_SCALE)?)
    } else {
        p.none(5).map(|_| None)?
    };
    let counter = if kind == Kind::Counter {
        Some(CounterRules {
            wrap_at: p.opt_u64_of(10, p.need(10)?)?,
            reset_ceiling_ppm: p.u32(11)?,
        })
    } else {
        p.none(10)?;
        p.none(11)?;
        None
    };
    let state_names: Vec<&str> = p
        .all(13)
        .map(|v| p.text(13, v, 1, 63))
        .collect::<Result<_, _>>()?;
    // Distinct, so a state's index and its name count the same transitions (Q-182). Past the limit the verdict is the
    // same either way, so the names are not looked at; a set rather than a sort keeps the cost linear (Q-651).
    let distinct = state_names.len() > MAX_STATE_NAMES || {
        let mut seen = std::collections::HashSet::with_capacity(state_names.len());
        state_names.iter().all(|s| seen.insert(*s))
    };
    if (value_type == ValueType::State) == state_names.is_empty()
        || state_names.len() > MAX_STATE_NAMES
        || !distinct
    {
        return Err(if state_names.len() > MAX_STATE_NAMES || !distinct {
            p.bad(13)
        } else {
            p.presence(13)
        });
    }
    let interval = if kind == Kind::Interval {
        Some(match p.u8_in(16, 2)? {
            0 => {
                let (length, phase) = (p.i64(17)?, p.i64(18)?);
                if length <= 0 {
                    return Err(p.bad(17));
                }
                if !(0..length).contains(&phase) {
                    return Err(p.bad(18));
                }
                p.none(19)?;
                p.none(22)?;
                IntervalLength::Fixed { length, phase }
            }
            tag => {
                p.none(17)?;
                p.none(18)?;
                let zone = p.own(p.need_text(19, 1, 63)?);
                if tag == 1 {
                    let day_start = p.i64(22)?;
                    if !(0..DAY).contains(&day_start) {
                        return Err(p.bad(22));
                    }
                    IntervalLength::Day { zone, day_start }
                } else {
                    p.none(22)?;
                    IntervalLength::Month { zone }
                }
            }
        })
    } else {
        for id in [16, 17, 18, 19, 22] {
            p.none(id)?;
        }
        None
    };
    let retention = match p.u8_in(23, 1)? {
        0 => p.none(24).map(|_| Retention::LongTerm)?,
        _ => Retention::Diagnostic { period: p.i64(24)? },
    };
    let excess_precision = if decimal {
        Some(match p.u8_in(25, 2)? {
            0 => p.none(26).map(|_| ExcessPrecision::Round)?,
            1 => p.none(26).map(|_| ExcessPrecision::Refuse)?,
            _ => {
                let max_scale = p.u8_in(26, MAX_SCALE)?;
                if Some(max_scale) < scale {
                    return Err(p.bad(26));
                }
                ExcessPrecision::AutoRaise { max_scale }
            }
        })
    } else {
        p.none(25)?;
        p.none(26)?;
        None
    };
    Ok(Definition {
        id: p.u32(1)?,
        name: p.own(p.need_text(2, 1, 255)?),
        kind,
        value_type,
        scale,
        unit: p.own(p.need_text(6, 0, 63)?),
        expected_interval: p.opt_i64_of(7, p.need(7)?, true)?,
        staleness: p.opt_i64_of(8, p.need(8)?, true)?,
        record_on_change: p.u8_in(9, 1)? == 1,
        counter,
        lateness_bound: p.i64(12)?,
        state_names: state_names.iter().map(|s| p.own(s)).collect(),
        versioning: match p.u8_in(14, 2)? {
            0 => Versioning::None,
            1 => Versioning::Time,
            _ => Versioning::Version,
        },
        timing: match p.u8_in(15, 2)? {
            0 => Timing::Observed,
            1 => Timing::Scheduled,
            _ => Timing::Forecast,
        },
        interval,
        retention,
        excess_precision,
        staleness_set: p.u8_in(27, 1)? == 1,
    })
}
