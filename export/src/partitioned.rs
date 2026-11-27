//! The partitioned layout's exports (`docs/export.md` Partitioned layout, Q-196): one read of the range for every
//! series asked for, in event-time order, so that each month's files are written as the read passes the month
//! (Q-1131).

use std::collections::BTreeMap;
use std::io::Write;

use kvik::{Position, Reader, SeriesId, TimeRange};

use crate::layout::{self, Month, file_name, partial_file_name, partition_dir};
use crate::{
    ExportError, ExportOptions, ExportRow, PartitionFile, SeriesSchema, SeriesWriter, with_store,
};

/// Exports one series' facts in `options.range` and `options.view`, one file per UTC month that holds a fact, at
/// `site=<site>/area=<s1>/device=<s2>/metric=<s3>/month=<YYYY-MM>/<view>.parquet` (`docs/export.md` Partitioned
/// layout, Q-196). `open` is given each file's relative path in month order; `options.site` is required, and a
/// series name without three non-empty segments is refused before anything is read (Q-670). The range is read once.
pub fn export_partitioned<W, F>(
    reader: &Reader,
    id: SeriesId,
    options: &ExportOptions,
    open: F,
) -> Result<Vec<PartitionFile>, ExportError>
where
    W: Write + Send,
    F: FnMut(&str) -> std::io::Result<W>,
{
    export_partitioned_of(reader, &[id], options, open)
}

/// Several series' [`export_partitioned`] from one read of the range (Q-1131): a series' file for a month is open from
/// its first fact in the month until the read passes the month, so every series with facts in a month has its file
/// open at once, each within the options' bounds. `open` is called in the order of each series' first fact in the
/// month, not of `ids`; the returned files come in month order, then in the order of `ids`, a repeated id once. Every
/// series is checked before anything is read. On failure every file then open has no footer.
pub fn export_partitioned_of<W, F>(
    reader: &Reader,
    ids: &[SeriesId],
    options: &ExportOptions,
    mut open: F,
) -> Result<Vec<PartitionFile>, ExportError>
where
    W: Write + Send,
    F: FnMut(&str) -> std::io::Result<W>,
{
    let options = &with_store(reader, options)?;
    let site = options.site.as_deref().ok_or(ExportError::InvalidOptions {
        reason: "a partitioned export needs a site",
    })?;
    let name = file_name(options.view)?;
    let mut set: Vec<SeriesId> = Vec::with_capacity(ids.len());
    for id in ids {
        if !set.contains(id) {
            set.push(*id);
        }
    }
    let mut schemas = BTreeMap::new();
    for (k, id) in set.iter().enumerate() {
        let info = reader.series(*id)?;
        let schema = SeriesSchema::from_info(&info)?;
        layout::checked_segments(&info.name)?;
        schemas.insert(*id, (k, schema, info.name));
    }
    let mut files = Vec::new();
    let mut month: Option<Month> = None;
    let mut writing: BTreeMap<usize, (SeriesWriter<W>, String)> = BTreeMap::new();
    for f in reader.facts_of(&set, options.range, options.view)? {
        let f = f?;
        let m = Month::of(f.t);
        check_month_order(month, m, f.position)?;
        if month != Some(m) {
            if let Some(done) = month {
                finish(&mut writing, done, &mut files)?;
            }
            month = Some(m);
        }
        let (k, schema, series_name) = &schemas[&f.series];
        let w = match writing.entry(*k) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(e) => {
                let r = m.range();
                let range =
                    TimeRange::new(r.from.max(options.range.from), r.to.min(options.range.to));
                let dir = partition_dir(site, series_name, m)?;
                let path = if range == r {
                    format!("{dir}/{name}")
                } else {
                    format!("{dir}/{}", partial_file_name(options.view, range)?)
                };
                let out = open(&path).map_err(|source| ExportError::Open {
                    path: path.clone(),
                    source,
                })?;
                e.insert((
                    SeriesWriter::new(schema, &options.clone().range(range), out)?,
                    path,
                ))
            }
        };
        w.0.push(&ExportRow::from(f))?;
    }
    if let Some(done) = month {
        finish(&mut writing, done, &mut files)?;
    }
    Ok(files)
}

/// Refuses a fact of a month before the one being written: its path was already returned (docs/export.md Publishing).
fn check_month_order(
    current: Option<Month>,
    next: Month,
    position: Position,
) -> Result<(), ExportError> {
    match current {
        Some(done) if next < done => Err(ExportError::MonthRegressed { position }),
        _ => Ok(()),
    }
}

/// Closes the files of `month`, in the order of their series.
fn finish<W: Write + Send>(
    writing: &mut BTreeMap<usize, (SeriesWriter<W>, String)>,
    month: Month,
    files: &mut Vec<PartitionFile>,
) -> Result<(), ExportError> {
    for (_, (w, path)) in std::mem::take(writing) {
        files.push(PartitionFile {
            path,
            month,
            summary: w.finish()?,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvik::Timestamp;

    fn month(t: i64) -> Month {
        Month::of(Timestamp(t))
    }

    #[test]
    fn a_regress_to_an_earlier_month_is_refused() {
        let later = month(1_800_000_000_000_000_000);
        let earlier = month(1_700_000_000_000_000_000);
        let p = Position::default();
        assert!(matches!(
            check_month_order(Some(later), earlier, p),
            Err(ExportError::MonthRegressed { .. })
        ));
        assert!(check_month_order(Some(earlier), later, p).is_ok());
        assert!(check_month_order(Some(later), later, p).is_ok());
        assert!(check_month_order(None, earlier, p).is_ok());
    }
}
