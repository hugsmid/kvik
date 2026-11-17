//! Series names (spec §2.1 `name`, Q-670): three non-empty `/`-separated
//! segments at definition and at alias.

mod common;

use common::T0;
use kvik_model::engine::{Config, Model};
use kvik_model::registry::RegistryError;
use kvik_model::types::{HOUR, Kind, SECOND, ValueType};

const REFUSED: [&str; 8] = [
    "",
    "plant",
    "plant/power",
    "a/b/c/d",
    "a//c",
    "/b/c",
    "a/b/",
    "//",
];

#[test]
fn s2_1_a_name_without_three_non_empty_segments_is_refused_at_definition() {
    let mut m = Model::new(Config::default(), vec![], T0 + HOUR);
    for name in REFUSED {
        assert_eq!(
            m.define(name, Kind::Gauge, ValueType::Number, "W", 10 * SECOND),
            Err(RegistryError::BadName(name.into()).into()),
            "{name:?}"
        );
    }
    assert_eq!(m.registry().unwrap().all().count(), 0, "no id taken");
    m.define(
        "plant/*/power",
        Kind::Gauge,
        ValueType::Number,
        "W",
        10 * SECOND,
    )
    .expect("a segment may hold `*` (Q-88)");
}

#[test]
fn s2_1_an_alias_without_three_non_empty_segments_is_refused() {
    let mut m = Model::new(Config::default(), vec![], T0 + HOUR);
    let s = m
        .define(
            "plant/line-1/power",
            Kind::Gauge,
            ValueType::Number,
            "W",
            10 * SECOND,
        )
        .unwrap();
    for name in REFUSED {
        assert_eq!(
            m.rename(s, name),
            Err(RegistryError::BadName(name.into()).into()),
            "{name:?}"
        );
    }
    m.rename(s, "plant/line-1/watts").unwrap();
    let names = m.registry().unwrap().get(s).unwrap().names.clone();
    assert_eq!(names, ["plant/line-1/power", "plant/line-1/watts"]);
}
