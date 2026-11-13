//! Records a few readings, a late correction and a counter reset, then reads
//! windows from the summary tree.

use kvik_model::engine::{Config, ConsumerDecl, ConsumerId, Model, Selection};
use kvik_model::summary::Aggregate;
use kvik_model::types::{Kind, MINUTE, Reading, SECOND, ValueType};

fn main() {
    let start = 1_700_000_000 * SECOND;
    let consumer = ConsumerDecl {
        id: ConsumerId(1),
        selection: Selection::All,
        capacity: 16,
    };
    let mut m = Model::new(Config::default(), vec![consumer], start);
    let temperature = m
        .define(
            "plant/line-1/temperature",
            Kind::Gauge,
            ValueType::Number,
            "degC",
            30 * SECOND,
        )
        .expect("define");
    let volume = m
        .define(
            "site/pump-2/volume",
            Kind::Counter,
            ValueType::Number,
            "l",
            30 * SECOND,
        )
        .expect("define");

    m.advance(MINUTE);
    m.ingest(vec![
        Reading::num(temperature, start + 10 * SECOND, 20.0),
        Reading::num(temperature, start + 20 * SECOND, 30.0),
        Reading::num(volume, start + 10 * SECOND, 1000.0),
        Reading::num(volume, start + 20 * SECOND, 1002.0),
        Reading::num(volume, start + 30 * SECOND, 1.0), // reset
    ])
    .expect("commit");
    // A correction of the first temperature reading.
    m.ingest(vec![Reading::num(temperature, start + 10 * SECOND, 25.0)])
        .expect("commit");

    let w = m
        .window(temperature, start, start + MINUTE)
        .expect("window");
    println!(
        "temperature: count {} coverage {} s avg {:?} max {:?}",
        w.count,
        w.coverage / i128::from(SECOND),
        w.aggregate(&Aggregate::Avg).and_then(|v| v.to_f64()),
        w.aggregate(&Aggregate::Max).and_then(|v| v.to_f64()),
    );
    let v = m.window(volume, start, start + MINUTE).expect("window");
    println!(
        "volume: increase {:?}",
        v.aggregate(&Aggregate::Increase).and_then(|x| x.to_f64())
    );
    m.drain().expect("drain");
    println!("delivered: {:?}", m.observed[&ConsumerId(1)].len());
}
