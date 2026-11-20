//! The live commands: `latest --live` and `tail`, one live read in a session (Q-1010, Q-1050).

use std::collections::BTreeMap;
use std::io::Write;
use std::time::{Duration, Instant};

use kvik::{
    Answer, Appended, LiveEvent, LiveOptions, Part, Query, SeriesId, SeriesInfo, SessionOptions,
    UpdateBatch, Updated,
};

use crate::Error;
use crate::args::Format;
use crate::commands::{Ctx, selected};
use crate::out::{Cell, Table};
use crate::show::{self, position};

const LATEST: &[&str] = &[
    "cut", "event", "id", "name", "state", "t", "value", "quality", "stale", "position",
];
const TAIL: &[&str] = &[
    "cut", "event", "position", "id", "name", "t", "value", "quality",
];

/// What a live command knows of the series it shows.
struct Names(BTreeMap<SeriesId, SeriesInfo>);

impl Names {
    fn of(c: &Ctx<'_>) -> Result<Names, Error> {
        let snap = c.reader.snapshot()?;
        let mut m = BTreeMap::new();
        for id in selected(c, &snap)? {
            m.insert(id, snap.series(id)?);
        }
        Ok(Names(m))
    }
}

/// A row that names no fact: `event` with `what` in the position column, the rest empty.
fn marker(head: &[&str], cut: &str, event: &str, what: String) -> Vec<Cell> {
    head.iter()
        .map(|h| match *h {
            "cut" => cut.into(),
            "event" => event.into(),
            "position" => Cell::Text(what.clone()),
            _ => Cell::Null,
        })
        .collect()
}

/// The rows a lifecycle event gives, the same for both commands.
fn common(head: &[&str], cut: &str, e: &LiveEvent, names: &mut Names) -> Option<Vec<Vec<Cell>>> {
    let one = |event: &str, what: String| Some(vec![marker(head, cut, event, what)]);
    match e {
        LiveEvent::Reset { above } => one("reset", format!("above {}", position(*above))),
        LiveEvent::Gap { first, last } => {
            one("gap", format!("{}..{}", position(*first), position(*last)))
        }
        LiveEvent::Lagged { from, to } => {
            one("lagged", format!("{}..{}", position(*from), position(*to)))
        }
        LiveEvent::Left { series } => {
            names.0.remove(series);
            one("left", format!("series {}", series.0))
        }
        LiveEvent::Closed(r) => one("closed", show::lower(r)),
        _ => None,
    }
}

/// Runs a session of one live read, writing each batch's rows; `rows` turns an event into rows.
fn run(
    c: &Ctx<'_>,
    query: Query,
    head: &'static [&'static str],
    out: &mut dyn Write,
    mut rows: impl FnMut(&str, &LiveEvent, &mut Names) -> Vec<Vec<Cell>>,
) -> Result<(), Error> {
    let mut names = Names::of(c)?;
    let interval = c.args.interval;
    let options = SessionOptions::new(interval, (interval / 2).max(Duration::from_millis(10)));
    let mut session = c.reader.session_with(options)?;
    session.open(query, LiveOptions::default())?;
    let until = c.args.run_for.map(|d| Instant::now() + d);
    let mut headed = false;
    loop {
        let left = until.map(|u| u.saturating_duration_since(Instant::now()));
        let wait = left
            .unwrap_or(Duration::from_secs(1))
            .min(Duration::from_secs(1));
        if let Some(b) = session.next(wait)? {
            let t = batch(&b, head, &mut names, &mut rows);
            let head_now = match c.args.format {
                Format::Table => true,
                Format::Csv => !headed,
                Format::Jsonl => false,
            };
            t.write(c.args.format, head_now, out)?;
            if c.args.format == Format::Table {
                writeln!(out)?;
            }
            out.flush()?;
            headed = true;
        }
        if left.is_some_and(|l| l.is_zero()) {
            return Ok(());
        }
    }
}

fn batch(
    b: &UpdateBatch,
    head: &'static [&'static str],
    names: &mut Names,
    rows: &mut impl FnMut(&str, &LiveEvent, &mut Names) -> Vec<Vec<Cell>>,
) -> Table {
    let cut = position(b.durable);
    let mut t = Table::new(head);
    for (_, e) in &b.events {
        for r in common(head, &cut, e, names).unwrap_or_else(|| rows(&cut, e, names)) {
            t.row(r);
        }
    }
    t
}

pub fn latest(c: &Ctx<'_>, out: &mut dyn Write) -> Result<(), Error> {
    let clock = c.clock.clone();
    let with = |cut: &str, event: &str, cells: Vec<Cell>| {
        let mut r = vec![cut.into(), event.into()];
        r.extend(cells);
        r
    };
    run(
        c,
        Query::Latest(c.selection()),
        LATEST,
        out,
        |cut, e, names| match e {
            LiveEvent::Opened(Answer::Latest(v)) => v
                .iter()
                .map(|(id, l)| with(cut, "opened", show::latest(names.0.get(id), *id, l, &clock)))
                .collect(),
            LiveEvent::Updated(Updated::Live { series, live }) => vec![with(
                cut,
                "updated",
                show::latest(names.0.get(series), *series, live, &clock),
            )],
            LiveEvent::Joined {
                series,
                info,
                part: Part::Live(l),
                ..
            } => {
                names.0.insert(*series, (**info).clone());
                vec![with(
                    cut,
                    "joined",
                    show::latest(Some(info), *series, l, &clock),
                )]
            }
            _ => Vec::new(),
        },
    )
}

pub fn tail(c: &Ctx<'_>, out: &mut dyn Write) -> Result<(), Error> {
    let clock = c.clock.clone();
    let query = Query::Tail { sel: c.selection() };
    run(c, query, TAIL, out, |cut, e, names| match e {
        LiveEvent::Opened(_) => vec![marker(TAIL, cut, "opened", String::new())],
        LiveEvent::Appended(Appended::Facts(facts)) => facts
            .iter()
            .map(|f| {
                let mut r = vec![cut.into(), "fact".into()];
                r.extend(show::fact(f, names.0.get(&f.series), &clock));
                r
            })
            .collect(),
        LiveEvent::Joined { series, info, .. } => {
            names.0.insert(*series, (**info).clone());
            vec![marker(TAIL, cut, "joined", format!("series {}", series.0))]
        }
        _ => Vec::new(),
    })
}
