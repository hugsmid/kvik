//! The canonical encoder of entries bodies (`format.md` A.6).

use super::*;

/// Writes a field list (A.6 framing).
struct Fields(Vec<u8>);

impl Fields {
    fn raw(&mut self, id: u16, v: &[u8]) {
        self.0.extend_from_slice(&id.to_le_bytes());
        self.0
            .extend_from_slice(&(v.len().min(u16::MAX as usize) as u16).to_le_bytes());
        self.0.extend_from_slice(v);
        self.0.resize(self.0.len().div_ceil(4) * 4, 0);
    }
    fn u8(&mut self, id: u16, v: u8) {
        self.raw(id, &[v]);
    }
    fn u32(&mut self, id: u16, v: u32) {
        self.raw(id, &v.to_le_bytes());
    }
    fn u64(&mut self, id: u16, v: u64) {
        self.raw(id, &v.to_le_bytes());
    }
    fn i64(&mut self, id: u16, v: i64) {
        self.raw(id, &v.to_le_bytes());
    }
    fn opt_i64(&mut self, id: u16, v: Option<i64>) {
        self.raw(
            id,
            &v.map(i64::to_le_bytes).unwrap_or_default()[..v.map_or(0, |_| 8)],
        );
    }
    fn opt_u64(&mut self, id: u16, v: Option<u64>) {
        self.raw(
            id,
            &v.map(u64::to_le_bytes).unwrap_or_default()[..v.map_or(0, |_| 8)],
        );
    }
    fn str(&mut self, id: u16, v: &str) {
        self.raw(id, v.as_bytes());
    }
    /// A selection at `kind` (53 or 60): the kind, the ids at `kind + 1`, the pattern at `kind + 2`.
    fn selection(&mut self, kind: u16, s: &Selection) {
        match s {
            Selection::None => self.u8(kind, 0),
            Selection::Ids(ids) => {
                self.u8(kind, 1);
                for id in ids {
                    self.u32(kind + 1, *id);
                }
            }
            Selection::Pattern { pattern, resolved } => {
                self.u8(kind, 2);
                for id in resolved {
                    self.u32(kind + 1, *id);
                }
                self.str(kind + 2, pattern);
            }
            Selection::All => self.u8(kind, 3),
        }
    }
    /// Fields 67 to 77 of a watch or unwatch; 54 or 55 are written with the selection.
    fn gate(&mut self, g: &RouteGate) {
        self.u32(67, g.id);
        let Some(s) = &g.spec else { return };
        self.str(68, &s.name);
        self.u8(
            69,
            match s.selector {
                GateSelector::One(_) => 1,
                GateSelector::Each(_) => 2,
            },
        );
        self.u8(71, s.input);
        match &s.kind {
            GateKind::Levels(ls) => {
                self.u8(72, 1);
                for l in ls {
                    let mut v = Vec::with_capacity(32);
                    v.extend_from_slice(&l.at.to_bits().to_le_bytes());
                    v.extend_from_slice(&l.clear.to_bits().to_le_bytes());
                    v.extend_from_slice(&l.hold_up.to_le_bytes());
                    v.extend_from_slice(&l.hold_down.to_le_bytes());
                    self.raw(73, &v);
                }
            }
            GateKind::Set {
                members,
                hold_on,
                hold_off,
            } => {
                self.u8(72, 2);
                for m in members {
                    let mut v = Vec::with_capacity(10);
                    match m {
                        GateValue::F64(x) => {
                            v.push(1);
                            v.extend_from_slice(&x.to_bits().to_le_bytes());
                        }
                        GateValue::Decimal { units, scale } => {
                            v.push(2);
                            v.extend_from_slice(&units.to_le_bytes());
                            v.push(*scale);
                        }
                        GateValue::State(i) => {
                            v.push(3);
                            v.extend_from_slice(&i.to_le_bytes());
                        }
                        GateValue::Bool(b) => v.extend_from_slice(&[4, u8::from(*b)]),
                        GateValue::Text(t) => {
                            v.push(5);
                            v.extend_from_slice(t.as_bytes());
                        }
                    }
                    self.raw(74, &v);
                }
                let mut v = hold_on.to_le_bytes().to_vec();
                v.extend_from_slice(&hold_off.to_le_bytes());
                self.raw(75, &v);
            }
            GateKind::Change { deadband } => {
                self.u8(72, 3);
                self.u64(76, deadband.to_bits());
            }
        }
        if let Some(n) = s.stale_after {
            self.i64(77, n);
        }
    }
}

pub(super) fn encode_entry(e: &Entry) -> (u8, Vec<u8>) {
    let mut f = Fields(Vec::new());
    let t = match e {
        Entry::Definition(d) => {
            f.u32(1, d.id);
            f.str(2, &d.name);
            f.u8(3, d.kind as u8);
            f.u8(4, d.value_type.code());
            if let Some(s) = d.scale {
                f.u8(5, s);
            }
            f.str(6, &d.unit);
            f.opt_i64(7, d.expected_interval);
            f.opt_i64(8, d.staleness);
            f.u8(9, u8::from(d.record_on_change));
            if let Some(c) = d.counter {
                f.opt_u64(10, c.wrap_at);
                f.u32(11, c.reset_ceiling_ppm);
            }
            f.i64(12, d.lateness_bound);
            for n in &d.state_names {
                f.str(13, n);
            }
            f.u8(14, d.versioning as u8);
            f.u8(15, d.timing as u8);
            match &d.interval {
                None => {}
                Some(IntervalLength::Fixed { length, phase }) => {
                    f.u8(16, 0);
                    f.i64(17, *length);
                    f.i64(18, *phase);
                }
                Some(IntervalLength::Day { zone, day_start }) => {
                    f.u8(16, 1);
                    f.str(19, zone);
                    f.i64(22, *day_start);
                }
                Some(IntervalLength::Month { zone }) => {
                    f.u8(16, 2);
                    f.str(19, zone);
                }
            }
            match d.retention {
                Retention::LongTerm => f.u8(23, 0),
                Retention::Diagnostic { period } => {
                    f.u8(23, 1);
                    f.i64(24, period);
                }
            }
            match d.excess_precision {
                None => {}
                Some(ExcessPrecision::Round) => f.u8(25, 0),
                Some(ExcessPrecision::Refuse) => f.u8(25, 1),
                Some(ExcessPrecision::AutoRaise { max_scale }) => {
                    f.u8(25, 2);
                    f.u8(26, max_scale);
                }
            }
            f.u8(27, u8::from(d.staleness_set));
            entry_type::DEFINITION
        }
        Entry::ParameterChange(p) => {
            f.u32(1, p.id);
            if let Some(v) = p.expected_interval {
                f.opt_i64(7, v);
            }
            if let Some((s, _)) = p.staleness {
                f.opt_i64(8, s);
            }
            if let Some(v) = p.record_on_change {
                f.u8(9, u8::from(v));
            }
            if let Some(v) = p.wrap_at {
                f.opt_u64(10, v);
            }
            if let Some(v) = p.reset_ceiling_ppm {
                f.u32(11, v);
            }
            if let Some(v) = p.lateness_bound {
                f.i64(12, v);
            }
            if let Some(v) = p.effective {
                f.i64(20, v);
            }
            if let Some(v) = p.retention_class {
                f.u8(23, v);
            }
            if let Some(v) = p.retention_period {
                f.i64(24, v);
            }
            match p.excess_precision {
                None => {}
                Some(ExcessPrecision::Round) => f.u8(25, 0),
                Some(ExcessPrecision::Refuse) => f.u8(25, 1),
                Some(ExcessPrecision::AutoRaise { max_scale }) => {
                    f.u8(25, 2);
                    f.u8(26, max_scale);
                }
            }
            if let Some((_, set)) = p.staleness {
                f.u8(27, u8::from(set));
            }
            entry_type::PARAMETER_CHANGE
        }
        Entry::Alias { id, name } => {
            f.u32(1, *id);
            f.str(2, name);
            entry_type::ALIAS
        }
        Entry::Retirement { id, t_r } => {
            f.u32(1, *id);
            f.i64(21, *t_r);
            entry_type::RETIREMENT
        }
        Entry::Routing(r) => {
            f.str(50, &r.consumer);
            f.u8(51, r.op as u8);
            if let Some(id) = r.funnel {
                f.u32(52, id);
            }
            if let Some(sel) = &r.selection {
                f.selection(53, sel);
            }
            match r
                .gate
                .as_ref()
                .and_then(|g| g.spec.as_ref())
                .map(|s| &s.selector)
            {
                Some(GateSelector::One(id)) => f.u32(54, *id),
                Some(GateSelector::Each(p)) => f.str(55, p),
                None => {}
            }
            if let Some(op) = &r.operator {
                f.u8(56, op.aggregate);
                f.i64(57, op.width);
                match op.state {
                    None => {}
                    Some(StateArg::Bool(b)) => f.raw(58, &[0, u8::from(b), 0, 0, 0]),
                    Some(StateArg::State(i)) => {
                        let mut v = vec![1];
                        v.extend_from_slice(&i.to_le_bytes());
                        f.raw(58, &v);
                    }
                }
            }
            f.selection(60, &r.emits);
            match r.condition {
                None => {}
                Some(RouteCondition::Threshold { on, clear }) => {
                    f.u8(63, 1);
                    f.u64(64, on.to_bits());
                    f.u64(65, clear.to_bits());
                }
                Some(RouteCondition::OnChange { deadband }) => {
                    f.u8(63, 2);
                    f.u64(64, deadband.to_bits());
                }
                Some(RouteCondition::HeldFor { above, n }) => {
                    f.u8(63, 3);
                    f.u64(64, above.to_bits());
                    f.i64(66, n);
                }
                Some(RouteCondition::StaleFor { n }) => {
                    f.u8(63, 4);
                    f.i64(66, n);
                }
            }
            if let Some(g) = &r.gate {
                f.gate(g);
            }
            entry_type::ROUTING
        }
        Entry::SiteZone { zone, effective } => {
            f.i64(20, *effective);
            f.str(30, zone);
            entry_type::SITE_ZONE
        }
        Entry::Tzdata { version } => {
            f.str(31, version);
            entry_type::TZDATA
        }
        Entry::Recovery(r) => {
            f.u64(40, r.after);
            if let Some(st) = r.stamp {
                f.raw(41, &st.encode());
            }
            f.raw(42, &r.boot_id);
            f.u64(43, r.tail_bytes);
            f.u32(44, r.readable_commits);
            f.u8(45, r.mark_state as u8);
            f.u32(46, r.id_floor);
            for c in &r.consumers {
                f.str(47, c);
            }
            entry_type::RECOVERY
        }
        Entry::ScaleRaise { id, scale } => {
            f.u32(1, *id);
            f.u8(5, *scale);
            entry_type::SCALE_RAISE
        }
        Entry::Ignored { .. } => unreachable!("ignored entries are never encoded"),
    };
    (t, f.0)
}
