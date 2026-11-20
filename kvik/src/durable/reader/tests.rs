use super::*;
use crate::env::ManualClock;
use crate::read::View as Shown;
use crate::series::{Kind, SeriesSpec, ValueType};
use crate::time::TimeRange;
use crate::types::{Reading, SeriesId, Value};
use crate::{Store, StoreConfig};

#[test]
fn a_first_read_in_parts_builds_the_index_one_read_whole_does() {
    const T0: i64 = 1_767_225_600_000_000_000;
    const S: i64 = 1_000_000_000;
    let env = Env::test(ManualClock::new(Timestamp(T0 + 4_000 * S)));
    let dir = Path::new("reader-parts");
    let config = StoreConfig::new(dir, "Etc/UTC")
        .create_if_missing(true)
        .env(env.clone());
    let s = crate::durable::tests::block_on(Store::open(config)).unwrap();
    let spec = SeriesSpec::new(
        "a/b/v",
        Kind::Gauge {
            value: ValueType::F64,
        },
        "W",
    );
    crate::durable::tests::block_on(s.define_series(spec)).unwrap();
    for chunk in (0..3_000i64).collect::<Vec<_>>().chunks(250) {
        let rs: Vec<Reading> = chunk
            .iter()
            .map(|&k| Reading::new(SeriesId(1), Timestamp(T0 + k * S), Value::F64(k as f64)))
            .collect();
        crate::durable::tests::block_on(s.emit(&rs)).unwrap();
    }
    crate::durable::tests::block_on(s.shutdown()).unwrap();
    let read = |part: usize| {
        let r = DurableReader::open(dir, &env, Duration::from_secs(60), Duration::from_secs(60))
            .unwrap();
        r.lock().part = part;
        let range = TimeRange::new(Timestamp(T0), Timestamp(T0 + 4_000 * S));
        let facts = r
            .with(|i, _| i.facts(SeriesId(1), range, Shown::History))
            .unwrap();
        let parts = r.lock().parts;
        (facts, parts)
    };
    let (parts, n) = read(100);
    let (whole, one) = read(usize::MAX);
    assert_eq!(parts.len(), 3_000);
    assert_eq!(parts, whole);
    // One read whole by the main reader at each of the call's two rounds, and one by the fact index.
    assert!(n >= 12 && one == 3, "{n} parts, {one} for the whole");
}

#[test]
fn a_copied_io_error_keeps_its_operating_system_code() {
    let e = QueryError::Io {
        path: PathBuf::from("a"),
        source: std::io::Error::from_raw_os_error(28),
    };
    let QueryError::Io { source, .. } = copy(&e) else {
        panic!("an I/O error")
    };
    assert_eq!(source.raw_os_error(), Some(28));
}
