//! The series registry (spec §2.1): series definitions, aliases, retirements, parameter changes, site-zone and
//! tzdata entries, built by applying the log's entries in position order, and the conversion between a
//! [`SeriesSpec`] and the definition entry the log carries (`format.md` A.6).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kvik_log::entries::{
    CounterRules as LogCounter, Definition, Entry, IntervalLength as LogInterval, Kind as LogKind,
    Retention, Timing as LogTiming, Versioning,
};
use kvik_log::facts::ValueType as LogType;
use kvik_model as km;

use super::read::pmap::PMap;
use crate::backend::Error;
use crate::backend::convert;
use crate::series::{
    CalendarUnit, ChangedAtKind, CounterRules, IntervalLength, Kind, RetentionClass, SeriesSpec,
    StateValues, Timing, ValueType,
};
use crate::time::{LocalTime, ZoneName};

/// The longest unit (field 6).
const MAX_UNIT: usize = 63;
/// The longest state name (field 13).
const MAX_STATE_NAME: usize = 63;

/// One series as the registry holds it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Series {
    /// The definition entry, with the retention class in force.
    pub(crate) def: Definition,
    /// The definition as the interface shows it.
    pub(crate) spec: SeriesSpec,
    /// Names in the order given; the last is the display name.
    pub(crate) names: Vec<String>,
    /// The effective time of its retirement.
    pub(crate) retired_at: Option<i64>,
}

impl Series {
    /// Whether the series is retired: it then leaves name resolution (spec §2.1).
    pub(crate) fn retired(&self) -> bool {
        self.retired_at.is_some()
    }
}

/// The registry: a function of the entries applied to it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Registry {
    /// Shared with the views a reader has published (Q-900).
    series: PMap<u32, Arc<Series>>,
    /// Site zones by effective time; of two at one time the later in the log.
    site_zones: BTreeMap<i64, String>,
    tzdata: Option<String>,
    /// The highest id known: defined, used by a fact or named as a recovery entry's id floor.
    highest: u32,
}

impl Registry {
    /// Applies an entry of the kept log.
    pub(crate) fn apply(&mut self, e: &Entry) {
        match e {
            Entry::Definition(d) => {
                self.highest = self.highest.max(d.id);
                self.series.insert(
                    d.id,
                    Arc::new(Series {
                        spec: spec_of(d),
                        names: vec![d.name.clone()],
                        retired_at: None,
                        def: (**d).clone(),
                    }),
                );
            }
            Entry::Alias { id, name } => {
                if let Some(s) = self.series.get_mut(id) {
                    Arc::make_mut(s).names.push(name.clone());
                }
            }
            Entry::Retirement { id, t_r } => {
                if let Some(s) = self.series.get_mut(id) {
                    Arc::make_mut(s).retired_at = Some(*t_r);
                }
            }
            Entry::ParameterChange(p) => {
                if let Some(s) = self.series.get_mut(&p.id) {
                    let s = Arc::make_mut(s);
                    match (p.retention_class, p.retention_period) {
                        (Some(0), _) => s.def.retention = Retention::LongTerm,
                        (Some(1), Some(period)) => {
                            s.def.retention = Retention::Diagnostic { period }
                        }
                        _ => {}
                    }
                    s.spec.retention = retention_out(s.def.retention);
                }
            }
            Entry::SiteZone { zone, effective } => {
                self.site_zones.insert(*effective, zone.clone());
            }
            Entry::Tzdata { version } => self.tzdata = Some(version.clone()),
            Entry::Recovery(r) => self.highest = self.highest.max(r.id_floor),
            // Decimal series are not supported yet, so no scale raise is written; an ignored entry changes nothing.
            Entry::ScaleRaise { .. } | Entry::Routing(_) | Entry::Ignored { .. } => {}
        }
    }

    /// Notes the series id of a kept fact: new ids are assigned above it (spec §2.1).
    pub(crate) fn note_fact(&mut self, id: u32) {
        self.highest = self.highest.max(id);
    }

    /// Every series, retired or not, in id order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Series> {
        self.series.values().map(|s| &**s)
    }

    /// A series by id, retired or not.
    pub(crate) fn get(&self, id: u32) -> Option<&Series> {
        self.series.get(&id).map(|s| &**s)
    }

    /// The live series that carries a name or alias.
    pub(crate) fn resolve(&self, name: &str) -> Option<&Series> {
        self.iter()
            .find(|s| !s.retired() && s.names.iter().any(|n| n == name))
    }

    /// The number of series defined.
    pub(crate) fn len(&self) -> usize {
        self.series.len()
    }

    /// The next id: above every id this registry knows and above `floor`, the writer's id floor (the highest id
    /// the mark, the registry file and the recovery entries name). Ids start at 1 (Q-121).
    pub(crate) fn next_id(&self, floor: u32) -> Option<u32> {
        self.highest.max(floor).checked_add(1)
    }

    /// The highest id this registry knows.
    pub(crate) fn highest(&self) -> u32 {
        self.highest
    }

    /// The site zone in force: that of the site-zone entry with the latest effective time, else `created`, the
    /// zone at creation (the store's zone, which an open must be configured with, `docs/api.md` Opening).
    pub(crate) fn site_zone<'a>(&'a self, created: &'a str) -> &'a str {
        self.site_zones
            .values()
            .next_back()
            .map_or(created, String::as_str)
    }

    /// The site-zone entries by effective time (spec §5.7); of two at one time the later in the log.
    pub(crate) fn site_zones(&self) -> &BTreeMap<i64, String> {
        &self.site_zones
    }

    /// The tzdata version in force: the latest tzdata entry, else `created`.
    pub(crate) fn tzdata<'a>(&'a self, created: &'a str) -> &'a str {
        self.tzdata.as_deref().unwrap_or(created)
    }
}

fn nanos(d: Duration) -> Result<i64, Error> {
    convert::dur_nanos(d)
}

fn dur(n: i64) -> Duration {
    convert::dur(i128::from(n))
}

fn invalid(field: &'static str, reason: &str) -> Error {
    Error::Invalid {
        field,
        reason: reason.into(),
    }
}

/// The definition entry for `spec` under `id`, every default resolved (`format.md` A.6). Validates the
/// specification with the reference model's registry rules, so that both backends refuse the same definitions.
pub(crate) fn definition_of(
    spec: &SeriesSpec,
    id: u32,
    default_lateness: Duration,
) -> Result<Definition, Error> {
    convert::check_name(&spec.name)?;
    if spec.unit.len() > MAX_UNIT {
        return Err(invalid("unit", "at most 63 bytes"));
    }
    let d = convert::definition(spec)?;
    let planned = km::registry::Registry::default()
        .plan_define(
            &spec.name,
            d.kind,
            d.value_type,
            &spec.unit,
            d.staleness,
            d.options,
            usize::MAX,
        )
        .map_err(|e| convert::error(km::engine::Error::Registry(e)))?;
    let staleness = match planned {
        Ok(km::registry::RegistryEvent::Define { staleness, .. }) => staleness,
        _ => d.staleness,
    };
    let expected = d
        .options
        .expected_interval
        .or(km::registry::default_expected_interval(
            d.kind,
            d.options.interval,
        ));
    let (kind, value_type, state_names, interval) = match &spec.kind {
        Kind::Gauge { .. } => (LogKind::Gauge, LogType::F64, Vec::new(), None),
        Kind::Counter { .. } => (LogKind::Counter, LogType::F64, Vec::new(), None),
        Kind::State {
            values: StateValues::Bool,
        } => (LogKind::State, LogType::Bool, Vec::new(), None),
        Kind::State {
            values: StateValues::Named(names),
        } => {
            if names
                .iter()
                .any(|n| n.is_empty() || n.len() > MAX_STATE_NAME)
            {
                return Err(invalid("kind", "a state name is 1 to 63 bytes"));
            }
            (LogKind::State, LogType::State, names.clone(), None)
        }
        Kind::Text => (LogKind::Text, LogType::Text, Vec::new(), None),
        Kind::Interval { length, .. } => {
            let IntervalLength::Fixed { d, phase } = length else {
                return Err(Error::NotSupportedYet(
                    "calendar-length interval series (a local day or month)",
                ));
            };
            (
                LogKind::Interval,
                LogType::F64,
                Vec::new(),
                Some(LogInterval::Fixed {
                    length: nanos(*d)?,
                    phase: nanos(*phase)?,
                }),
            )
        }
    };
    let counter = (kind == LogKind::Counter).then(|| {
        let rules = spec.counter.unwrap_or_default();
        LogCounter {
            wrap_at: rules.wrap_at.map(f64::to_bits),
            // Checked to lie in 0..=1 by the model's rules above.
            reset_ceiling_ppm: (rules.reset_ceiling * 1_000_000.0).round() as u32,
        }
    });
    Ok(Definition {
        id,
        name: spec.name.clone(),
        kind,
        value_type,
        scale: None,
        unit: spec.unit.clone(),
        expected_interval: expected,
        staleness,
        record_on_change: spec.record_on_change,
        counter,
        lateness_bound: match d.options.lateness_bound {
            Some(l) => l,
            None => nanos(default_lateness)?,
        },
        state_names,
        versioning: match spec.versioned {
            None => Versioning::None,
            Some(ChangedAtKind::Time) => Versioning::Time,
            Some(ChangedAtKind::Version) => Versioning::Version,
        },
        timing: match spec.timing {
            Timing::Observed => LogTiming::Observed,
            Timing::Scheduled => LogTiming::Scheduled,
            Timing::Forecast => LogTiming::Forecast,
        },
        interval,
        retention: match d.retention {
            km::registry::RetentionClass::LongTerm => Retention::LongTerm,
            km::registry::RetentionClass::Diagnostic { period } => Retention::Diagnostic { period },
        },
        excess_precision: None,
        staleness_set: spec.staleness.is_some(),
    })
}

/// The retention class of the interface.
pub(crate) fn retention_out(r: Retention) -> RetentionClass {
    match r {
        Retention::LongTerm => RetentionClass::LongTerm,
        Retention::Diagnostic { period } => RetentionClass::Diagnostic {
            period: dur(period),
        },
    }
}

/// The interface's view of a definition entry: every field as the entry resolved it. The expected interval,
/// lateness bound and counter rules are given explicitly; the staleness limit only when the caller set it.
pub(crate) fn spec_of(d: &Definition) -> SeriesSpec {
    let number = || match (d.value_type, d.scale) {
        (LogType::Decimal, Some(scale)) => ValueType::Decimal { scale },
        (LogType::Decimal, None) => ValueType::decimal(),
        _ => ValueType::F64,
    };
    let kind = match d.kind {
        LogKind::Gauge => Kind::Gauge { value: number() },
        LogKind::Counter => Kind::Counter { value: number() },
        LogKind::State if d.value_type == LogType::Bool => Kind::State {
            values: StateValues::Bool,
        },
        LogKind::State => Kind::State {
            values: StateValues::Named(d.state_names.clone()),
        },
        LogKind::Text => Kind::Text,
        LogKind::Interval => Kind::Interval {
            value: number(),
            length: match &d.interval {
                Some(LogInterval::Fixed { length, phase }) => IntervalLength::Fixed {
                    d: dur(*length),
                    phase: dur(*phase),
                },
                Some(LogInterval::Day { zone, day_start }) => IntervalLength::Calendar {
                    unit: CalendarUnit::Day {
                        start: LocalTime::from_nanos(u64::try_from(*day_start).unwrap_or(0)),
                    },
                    zone: ZoneName::new(zone.clone()),
                },
                Some(LogInterval::Month { zone }) => IntervalLength::Calendar {
                    unit: CalendarUnit::Month,
                    zone: ZoneName::new(zone.clone()),
                },
                None => IntervalLength::Fixed {
                    d: Duration::ZERO,
                    phase: Duration::ZERO,
                },
            },
        },
    };
    let mut spec = SeriesSpec::new(d.name.clone(), kind, d.unit.clone());
    spec.expected_interval = d.expected_interval.map(dur);
    spec.staleness = if d.staleness_set {
        d.staleness.map(dur)
    } else {
        None
    };
    spec.record_on_change = d.record_on_change;
    spec.counter = d.counter.map(|c| {
        CounterRules::new(
            c.wrap_at.map(f64::from_bits),
            f64::from(c.reset_ceiling_ppm) / 1_000_000.0,
        )
    });
    spec.lateness_bound = Some(dur(d.lateness_bound));
    spec.versioned = match d.versioning {
        Versioning::None => None,
        Versioning::Time => Some(ChangedAtKind::Time),
        Versioning::Version => Some(ChangedAtKind::Version),
    };
    spec.timing = match d.timing {
        LogTiming::Observed => Timing::Observed,
        LogTiming::Scheduled => Timing::Scheduled,
        LogTiming::Forecast => Timing::Forecast,
    };
    spec.retention = retention_out(d.retention);
    spec
}

/// The first field in which a resolved definition differs from a series' definition in force, or `None` when
/// they define the same series (names aside).
pub(crate) fn difference(new: &Definition, old: &Definition) -> Option<&'static str> {
    if new.kind != old.kind
        || new.value_type != old.value_type
        || new.scale != old.scale
        || new.state_names != old.state_names
        || new.interval != old.interval
    {
        Some("kind")
    } else if new.unit != old.unit {
        Some("unit")
    } else if new.expected_interval != old.expected_interval {
        Some("expected_interval")
    } else if new.staleness != old.staleness || new.staleness_set != old.staleness_set {
        Some("staleness")
    } else if new.versioning != old.versioning {
        Some("versioned")
    } else if new.timing != old.timing {
        Some("timing")
    } else if new.record_on_change != old.record_on_change
        || new.counter != old.counter
        || new.lateness_bound != old.lateness_bound
        || new.retention != old.retention
        || new.excess_precision != old.excess_precision
    {
        Some("definition")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gauge() -> SeriesSpec {
        SeriesSpec::new(
            "a/b/c",
            Kind::Gauge {
                value: ValueType::F64,
            },
            "W",
        )
    }

    #[test]
    fn defaults_are_resolved_and_shown_back() {
        let d = definition_of(&gauge(), 1, Duration::from_secs(300)).unwrap();
        assert_eq!(d.expected_interval, Some(60_000_000_000));
        assert_eq!(d.staleness, Some(180_000_000_000));
        assert!(!d.staleness_set);
        assert_eq!(d.lateness_bound, 300_000_000_000);
        let spec = spec_of(&d);
        assert_eq!(spec.staleness, None);
        assert_eq!(spec.expected_interval, Some(Duration::from_secs(60)));
        // Resolving the shown specification again gives the same definition.
        assert_eq!(
            definition_of(&spec, 1, Duration::from_secs(1)).unwrap(),
            d,
            "the lateness bound is carried explicitly"
        );
    }

    #[test]
    fn ids_go_above_every_floor() {
        let mut r = Registry::default();
        assert_eq!(r.next_id(0), Some(1), "ids start at 1");
        assert_eq!(r.next_id(7), Some(8));
        r.note_fact(9);
        assert_eq!(r.next_id(7), Some(10));
        assert_eq!(r.next_id(u32::MAX), None);
    }

    #[test]
    fn differences_name_the_first_field() {
        let a = definition_of(&gauge(), 1, Duration::from_secs(300)).unwrap();
        let b = definition_of(
            &gauge().expected_interval(Duration::from_secs(2)),
            1,
            Duration::from_secs(300),
        )
        .unwrap();
        assert_eq!(difference(&b, &a), Some("expected_interval"));
        assert_eq!(difference(&a, &a), None);
    }

    #[test]
    fn a_definition_refuses_a_name_without_three_non_empty_segments() {
        for name in ["plant", "plant/power", "a/b/c/d", "a//c", "/b/c", "a/b/"] {
            let spec = SeriesSpec {
                name: name.into(),
                ..gauge()
            };
            assert!(
                matches!(
                    definition_of(&spec, 1, Duration::from_secs(300)),
                    Err(Error::Invalid { field: "name", .. })
                ),
                "{name:?}"
            );
        }
    }

    /// Q-670: a log written before the three-segment rule opens with names of another shape; such a series stays
    /// readable and writable by id, no pattern selects it through a name of another number of segments, `*` matches
    /// an empty segment, and only an alias of the right shape can be added, which patterns then select (spec §4).
    #[test]
    fn a_logged_name_of_another_shape_opens_and_stays_readable_by_id() {
        use std::path::PathBuf;
        use std::sync::Arc;
        use std::task::{Context, Poll, Waker};

        use super::super::{Emitted, Writer, facts, lock};
        use crate::config::StoreConfig;
        use crate::consume::{DeliveryKind, Funnel, Select, Start};
        use crate::env::{Env, ManualClock, MemFs};
        use crate::read::View;
        use crate::series::SeriesKey;
        use crate::time::Timestamp;
        use crate::types::{Intent, Reading, SeriesId, Value};

        fn emit(w: &Arc<Writer>, ids: &[SeriesId], t: Timestamp) {
            let r: Vec<Reading> = ids
                .iter()
                .map(|id| Reading::new(*id, t, Value::F64(1.0)))
                .collect();
            if let Emitted::Queued(mut q) = w.emit(&r, Intent::Normal, true).unwrap() {
                let mut cx = Context::from_waker(Waker::noop());
                loop {
                    if let Poll::Ready(r) = q.poll(&mut cx) {
                        q.finish(r).unwrap();
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
        fn delivered(w: &Writer, consumer: &str) -> Vec<SeriesId> {
            let mut got = Vec::new();
            while let Some(d) = w.poll_delivery(consumer, None).unwrap() {
                w.confirm(consumer, d.id).unwrap();
                if let DeliveryKind::Fact(f) = d.kind {
                    got.push(f.series);
                }
            }
            got
        }

        let clock = ManualClock::new("2026-01-05T12:00:00Z".parse().unwrap());
        let mut env = Env::test(clock.clone());
        env.fs = Arc::new(MemFs::new());
        env.background_timers = false;
        let config = StoreConfig::new(PathBuf::from("durable-unit/old-name"), "UTC")
            .create_if_missing(true)
            .env(env);
        let (old, empty) = ("plant/power", "x//z");
        let w = Writer::open(&config).unwrap();
        let line = w.define(&gauge()).unwrap();
        let mut ids = Vec::new();
        for (k, name) in [old, empty].into_iter().enumerate() {
            let mut def =
                definition_of(&gauge(), line.0 + 1 + k as u32, Duration::from_secs(300)).unwrap();
            def.name = name.into();
            ids.push(SeriesId(def.id));
            w.commit_entry_held(&mut lock(&w.decider), Entry::Definition(Box::new(def)))
                .unwrap();
        }
        let (id, e) = (ids[0], ids[1]);
        w.shutdown().unwrap();
        drop(w);

        let w = Writer::open(&config).unwrap();
        assert_eq!(w.series(&SeriesKey::Id(id)).unwrap().name, old);
        assert_eq!(w.series(&SeriesKey::Name(old.into())).unwrap().id, id);
        let t = clock.wall();
        emit(&w, &[id], t);
        w.shutdown().unwrap();
        drop(w);

        let w = Writer::open(&config).unwrap();
        let range = t..Timestamp(t.0 + 1);
        let got: Vec<_> = facts(&w.reader().unwrap(), id, range.into(), View::Current)
            .unwrap()
            .collect();
        assert_eq!(got.len(), 1, "readable by id");
        let again = SeriesSpec {
            name: old.into(),
            ..gauge()
        };
        assert!(matches!(
            w.define(&again),
            Err(Error::Invalid { field: "name", .. })
        ));
        assert!(matches!(
            w.alias(id, "plant/power-2"),
            Err(Error::Invalid { field: "name", .. })
        ));
        w.take_consumer("p", Start::Now, &Select::None).unwrap();
        w.route("p", &Funnel::facts(Select::Names("plant/*/power".into())))
            .unwrap();
        w.take_consumer("q", Start::Now, &Select::None).unwrap();
        w.route("q", &Funnel::facts(Select::Names("x/*/z".into())))
            .unwrap();
        emit(&w, &[line, id, e], Timestamp(t.0 + 1_000_000_000));
        assert_eq!(
            delivered(&w, "p"),
            [],
            "no pattern selects a name of two segments"
        );
        assert_eq!(delivered(&w, "q"), [e], "`*` matches an empty segment");
        w.alias(id, "plant/line-2/power").unwrap();
        let info = w.series(&SeriesKey::Id(id)).unwrap();
        assert_eq!(
            info.names,
            vec![old.to_string(), "plant/line-2/power".into()]
        );
        emit(&w, &[line, id, e], Timestamp(t.0 + 2_000_000_000));
        assert_eq!(delivered(&w, "p"), [id], "selected through its alias");
        w.release("p");
        w.release("q");
        w.shutdown().unwrap();
    }
}
