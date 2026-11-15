//! A large export pulls its rows lazily and hands bytes to its output as it goes, holding at most one row group
//! and one batch of rows that the output has not yet received.

mod common;

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::{f64_series, row};
use kvik::Value;
use kvik_export::{ExportOptions, export_rows};

/// Discards bytes, recording how many rows had been pulled at each write.
struct Sink {
    pulled: Arc<AtomicU64>,
    writes: Vec<u64>,
    bytes: u64,
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writes.push(self.pulled.load(Ordering::SeqCst));
        self.bytes += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_million_rows_stream_with_at_most_a_row_group_and_a_batch_unwritten() {
    const N: u64 = 1_000_000;
    const GROUP: usize = 65_536;
    const BATCH: usize = 4_096;
    let pulled = Arc::new(AtomicU64::new(0));
    let counter = pulled.clone();
    let rows = (0..N).map(move |i| {
        counter.fetch_add(1, Ordering::SeqCst);
        let secs = i64::try_from(i).unwrap();
        Ok(row(1, secs, i + 1, Value::F64(secs as f64 * 0.5)))
    });
    let mut sink = Sink {
        pulled: pulled.clone(),
        writes: Vec::new(),
        bytes: 0,
    };
    let options = ExportOptions::default()
        .row_group_rows(GROUP)
        .batch_rows(BATCH);
    let summary = export_rows(&f64_series(1), rows, &options, &mut sink).unwrap();

    assert_eq!(summary.rows, N);
    assert_eq!(summary.row_groups, N.div_ceil(GROUP as u64) as usize);
    let bound = (GROUP + BATCH) as u64;
    let mut seen = 0;
    for &w in sink.writes.iter().chain([&N]) {
        assert!(w - seen <= bound, "{} rows pulled between writes", w - seen);
        seen = w;
    }
    assert!(sink.writes.iter().any(|&w| w < N / 2));
    assert!(sink.bytes > 0);
}
