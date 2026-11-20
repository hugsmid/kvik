//! The static commands: `series`, `latest`, `health` and `info`.

use std::io::Write;

use kvik::env::Env;
use kvik::{Health, Live, Reader, Select, Selection, SeriesId, Snapshot};

use crate::args::Args;
use crate::files;
use crate::out::{Cell, Table, num};
use crate::show::{self, Clock, lower, position};
use crate::Error;

/// What every command works with.
pub struct Ctx<'a> {
    pub args: &'a Args,
    pub reader: &'a Reader,
    pub env: &'a Env,
    pub clock: &'a Clock,
}

impl Ctx<'_> {
    /// The series the patterns select; every series without one.
    pub fn selection(&self) -> Selection {
        if self.args.patterns.is_empty() {
            Selection::all()
        } else {
            Selection::new(self.args.patterns.iter().cloned().map(Select::Names))
        }
    }

    fn table(&self, t: &Table, out: &mut dyn Write) -> Result<(), Error> {
        t.write(self.args.format, true, out)?;
        Ok(())
    }
}

/// The selected series of `snap`, in id order.
pub fn selected(c: &Ctx<'_>, snap: &Snapshot) -> Result<Vec<SeriesId>, Error> {
    let mut ids = snap.select(&c.selection())?;
    ids.sort();
    Ok(ids)
}

pub fn series(c: &Ctx<'_>, out: &mut dyn Write) -> Result<(), Error> {
    let snap = c.reader.snapshot()?;
    let mut t = Table::new(&["id", "name", "kind", "unit", "latest_t", "names", "retired_at"]);
    for id in selected(c, &snap)? {
        let i = snap.series(id)?;
        t.row(vec![
            num(id.0),
            i.name.clone().into(),
            show::kind(&i.spec.kind).into(),
            i.spec.unit.clone().into(),
            c.clock.cell(i.latest_t),
            Cell::Text(i.names.join(" ")),
            c.clock.cell(i.retired_at),
        ]);
    }
    c.table(&t, out)
}

pub fn latest(c: &Ctx<'_>, out: &mut dyn Write) -> Result<(), Error> {
    let snap = c.reader.snapshot()?;
    let mut t = Table::new(show::LATEST);
    for id in selected(c, &snap)? {
        let info = snap.series(id)?;
        let live: Live = snap.live(id)?;
        t.row(show::latest(Some(&info), id, &live, c.clock));
    }
    c.table(&t, out)
}

const SECTIONED: &[&str] = &["section", "item", "value"];

fn add(t: &mut Table, section: &str, item: impl Into<Cell>, value: impl Into<Cell>) {
    t.row(vec![section.into(), item.into(), value.into()]);
}

/// The variant name of a `Debug` form.
fn variant(d: impl std::fmt::Debug) -> String {
    let s = format!("{d:?}");
    let end = s.find(['(', ' ', '{']).unwrap_or(s.len());
    lower(&s[..end]).trim_matches('"').to_owned()
}

pub fn health(c: &Ctx<'_>, out: &mut dyn Write) -> Result<(), Error> {
    let dir = &c.args.store;
    let fs = &*c.env.fs;
    let st = c.reader.refresh()?;
    let now = c.env.clock.wall();
    let mut t = Table::new(SECTIONED);
    match c.reader.health() {
        Health::Ok => add(&mut t, "reader", "health", "ok"),
        Health::Degraded { reasons } => {
            add(&mut t, "reader", "health", "degraded");
            for r in reasons {
                add(&mut t, "reader", "reason", variant(r));
            }
        }
        other => add(&mut t, "reader", "health", variant(other)),
    }
    add(&mut t, "reader", "durable", position(st.durable));
    add(&mut t, "reader", "generation", num(st.generation));
    add(&mut t, "reader", "epoch", num(st.epoch));
    add(
        &mut t,
        "reader",
        "reset_stamp",
        format!("{}/{:016x}", st.stamp.sequence, st.stamp.nonce),
    );
    for l in c.reader.losses() {
        let range = match (l.first, l.last) {
            (Some(f), Some(l)) => format!("{}..{}", position(f), position(l)),
            _ => "unknown".into(),
        };
        let state = match (l.recorded, l.acknowledged) {
            (_, true) => "acknowledged",
            (true, false) => "recorded",
            (false, false) => "found",
        };
        add(
            &mut t,
            "loss",
            position(l.identity),
            format!("{} {} {range} {state}", lower(l.kind), lower(l.cause)),
        );
    }
    let all = files::walk(fs, dir);
    let count = |ext: &str| {
        all.iter()
            .filter(|(p, _)| p.starts_with("log/") && p.ends_with(ext))
            .count()
    };
    add(&mut t, "log", "sealed_segments", num(count(".kvl")));
    add(&mut t, "log", "open_segments", num(count(".open")));
    let log_bytes: u64 = all
        .iter()
        .filter(|(p, _)| p.starts_with("log/"))
        .map(|(_, l)| *l)
        .sum();
    add(&mut t, "log", "bytes", num(log_bytes));
    let total: u64 = all.iter().map(|(_, l)| *l).sum();
    add(&mut t, "tier", "local_bytes", num(total));
    add(&mut t, "tier", "cold", "not configured");
    if let Some(d) = files::descriptor(fs, dir) {
        let leases = files::leases(fs, dir, &d.store_id);
        add(&mut t, "leases", "count", num(leases.len()));
        for (id, slot) in leases {
            let what = match slot {
                Some(s) => format!(
                    "epoch {} renewals {} created {}",
                    s.epoch,
                    s.refresh_counter,
                    c.clock.time(kvik::Timestamp(s.created_at))
                ),
                None => "invalid".into(),
            };
            add(&mut t, "lease", files::hex(&id), what);
        }
        match files::series_directory(fs, dir, &d.store_id) {
            Some((frame, s)) => {
                add(&mut t, "series_directory", "file_id", num(frame.file_id));
                add(
                    &mut t,
                    "series_directory",
                    "through",
                    format!("{}:{}", s.through.generation, s.through.sequence),
                );
                add(&mut t, "series_directory", "entries", num(s.entries.len()));
                add(
                    &mut t,
                    "series_directory",
                    "written_at",
                    c.clock.time(kvik::Timestamp(frame.written_at)),
                );
            }
            None => add(&mut t, "series_directory", "state", "none"),
        }
        match files::status(fs, dir, &d.store_id) {
            Some((frame, s)) => {
                let age = now.0.saturating_sub(frame.written_at) / 1_000_000;
                add(&mut t, "status", "age_ms", num(age));
                add(&mut t, "status", "health", files::health_name(s.health));
                add(
                    &mut t,
                    "status",
                    "pressure",
                    s.pressure_in_force(frame.written_at, now.0).to_string(),
                );
                for r in &s.reasons {
                    add(&mut t, "status", "reason", files::reason_name(*r));
                }
                for (id, v) in &s.counters {
                    add(&mut t, "status", files::counter_name(*id), num(v));
                }
            }
            None => add(&mut t, "status", "state", "none or invalid"),
        }
    }
    c.table(&t, out)
}

pub fn info(c: &Ctx<'_>, out: &mut dyn Write) -> Result<(), Error> {
    let dir = &c.args.store;
    let fs = &*c.env.fs;
    let mut t = Table::new(SECTIONED);
    if let Some(d) = files::descriptor(fs, dir) {
        add(&mut t, "store", "store_id", files::hex(&d.store_id));
        add(
            &mut t,
            "store",
            "created_at",
            c.clock.time(kvik::Timestamp(d.created_at)),
        );
        add(&mut t, "store", "site_zone_at_creation", d.site_zone.clone());
        add(&mut t, "store", "tzdata_at_creation", d.tzdata.clone());
        add(&mut t, "store", "software", d.software.clone());
        let (zone, tzdata) = files::in_force(fs, dir, &d);
        add(&mut t, "store", "site_zone", zone);
        add(&mut t, "store", "tzdata", tzdata);
    }
    let snap = c.reader.snapshot()?;
    add(&mut t, "store", "series", num(selected(c, &snap)?.len()));
    for (kind, (n, bytes)) in files::by_kind(&files::walk(fs, dir)) {
        add(&mut t, "files", kind, format!("{n} files, {bytes} bytes"));
    }
    c.table(&t, out)
}
