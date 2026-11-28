//! Routing entries (`format.md` A.6, type 5): funnels, conditions and gates.

use super::*;

pub(super) fn routing(p: &Parsed<'_>) -> Result<Routing, BodyError> {
    let v = p.need(50)?;
    let consumer = match std::str::from_utf8(v) {
        Ok(s) if consumer_name_ok(s) => p.own(s),
        _ => return Err(p.bad(50)),
    };
    let op = match p.u8_in(51, 5)? {
        1 => RouteOp::Add,
        2 => RouteOp::Remove,
        3 => RouteOp::Emits,
        4 => RouteOp::Watch,
        5 => RouteOp::Unwatch,
        _ => return Err(p.bad(51)),
    };
    let funnel = match op {
        RouteOp::Add | RouteOp::Remove => Some(p.u32(52)?),
        _ => p.none(52).map(|()| None)?,
    };
    let gate = match op {
        RouteOp::Watch | RouteOp::Unwatch => Some(Box::new(route_gate(p, op)?)),
        _ => {
            for id in GATE_FIELDS {
                p.none(id)?;
            }
            None
        }
    };
    let funnel_fields: &[u16] = match op {
        RouteOp::Watch => &[53, 56, 57, 58, 63, 64, 65, 66],
        _ => &[53, 54, 55, 56, 57, 58, 63, 64, 65, 66],
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
            p.none(*id)?;
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
        gate,
    })
}

const GATE_FIELDS: [u16; 10] = [67, 68, 69, 71, 72, 73, 74, 75, 76, 77];

/// Fields 67 to 77 (with 54 or 55) of a watch, or 67 alone of an unwatch.
fn route_gate(p: &Parsed<'_>, op: RouteOp) -> Result<RouteGate, BodyError> {
    let id = p.u32(67)?;
    if op == RouteOp::Unwatch {
        for f in &GATE_FIELDS[1..] {
            p.none(*f)?;
        }
        return Ok(RouteGate { id, spec: None });
    }
    let name = p.own(p.need_text(68, 1, 64)?);
    let selector = match p.u8_in(69, 2)? {
        1 => {
            p.none(55)?;
            let mut ids = p.all(54);
            let (Some(v), None) = (ids.next(), ids.next()) else {
                return Err(p.presence(54));
            };
            if v.len() != 4 {
                return Err(p.bad(54));
            }
            GateSelector::One(u32_at(v, 0))
        }
        2 => {
            p.none(54)?;
            GateSelector::Each(p.own(p.need_text(55, 1, 255)?))
        }
        _ => return Err(p.bad(69)),
    };
    // Spec §6 Gates: window inputs (2) are reserved until their fold exists.
    let input = p.u8_in(71, 1)?;
    if input != 1 {
        return Err(p.bad(71));
    }
    let kind = match p.u8_in(72, 3)? {
        1 => {
            for f in [74, 75, 76] {
                p.none(f)?;
            }
            let mut ls: Vec<GateLevel> = Vec::new();
            for v in p.all(73) {
                if v.len() != 32 {
                    return Err(p.bad(73));
                }
                let l = GateLevel {
                    at: f64::from_bits(u64_at(v, 0)),
                    clear: f64::from_bits(u64_at(v, 8)),
                    hold_up: u64_at(v, 16) as i64,
                    hold_down: u64_at(v, 24) as i64,
                };
                let rising = ls.last().is_none_or(|b| b.at < l.at && b.clear <= l.clear);
                if !(l.at.is_finite() && l.clear.is_finite())
                    || l.clear > l.at
                    || l.hold_up < 0
                    || l.hold_down < 0
                    || !rising
                    || ls.len() == GATE_LEVELS
                {
                    return Err(p.bad(73));
                }
                ls.push(l);
            }
            if ls.is_empty() {
                return Err(p.presence(73));
            }
            GateKind::Levels(ls)
        }
        2 => {
            for f in [73, 76] {
                p.none(f)?;
            }
            let mut members = Vec::new();
            for v in p.all(74) {
                if members.len() == GATE_MEMBERS {
                    return Err(p.bad(74));
                }
                members.push(gate_value(p, v).ok_or(p.bad(74))?);
            }
            if members.is_empty() {
                return Err(p.presence(74));
            }
            let h = p.need(75)?;
            if h.len() != 16 {
                return Err(p.bad(75));
            }
            let (hold_on, hold_off) = (u64_at(h, 0) as i64, u64_at(h, 8) as i64);
            if hold_on < 0 || hold_off < 0 {
                return Err(p.bad(75));
            }
            GateKind::Set {
                members,
                hold_on,
                hold_off,
            }
        }
        3 => {
            for f in [73, 74, 75] {
                p.none(f)?;
            }
            let deadband = finite(p, 76)?;
            if deadband < 0.0 {
                return Err(p.bad(76));
            }
            GateKind::Change { deadband }
        }
        _ => return Err(p.bad(72)),
    };
    let stale_after = match p.get(77) {
        None => None,
        Some(_) => match p.i64(77)? {
            n if n > 0 => Some(n),
            _ => return Err(p.bad(77)),
        },
    };
    Ok(RouteGate {
        id,
        spec: Some(GateSpec {
            name,
            selector,
            input,
            kind,
            stale_after,
        }),
    })
}

fn gate_value(p: &Parsed<'_>, v: &[u8]) -> Option<GateValue> {
    let (tag, rest) = v.split_first()?;
    Some(match (tag, rest.len()) {
        (1, 8) => {
            let x = f64::from_bits(u64_at(rest, 0));
            if !x.is_finite() {
                return None;
            }
            GateValue::F64(x)
        }
        (2, 9) if rest[8] <= MAX_SCALE => GateValue::Decimal {
            units: u64_at(rest, 0) as i64,
            scale: rest[8],
        },
        (3, 4) if (u32_at(rest, 0) as usize) < MAX_STATE_NAMES => GateValue::State(u32_at(rest, 0)),
        (4, 1) if rest[0] <= 1 => GateValue::Bool(rest[0] == 1),
        (5, 1..=255) => GateValue::Text(p.own(std::str::from_utf8(rest).ok()?)),
        _ => return None,
    })
}
