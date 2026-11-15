//! Golden stored nodes (`tests/golden/README.md`): each file is what the encoder writes now for the node built from
//! fixed readings, and decodes to it. Regenerate only after a deliberate format change:
//! `KVIK_BLESS_GOLDEN=1 cargo test -p kvik-tree --test golden`.

use std::fs;
use std::path::PathBuf;

use kvik_tree::{
    CounterRules, DecodeError, Kind, Node, Params, Value, decode, encode, fold, merge_all,
};

const T0: i64 = 1_767_225_600_000_000_000;
const SECOND: i64 = 1_000_000_000;
const MINUTE: i64 = 60 * SECOND;

fn at(s: i64, v: Value) -> (i64, Value) {
    (T0 + s * SECOND, v)
}

fn minute(p: &Params, r: &[(i64, Value)]) -> Node {
    fold(p, T0, T0 + MINUTE, r)
}

fn nodes() -> Vec<(&'static str, Node)> {
    let gauge = Params::new(Kind::Gauge, Some(20 * SECOND));
    let g = [21.5, -0.0, 0.1, 5e-324, 1e300, 21.37];
    let g: Vec<_> = (0..6)
        .map(|i| at(i * 9, Value::F64(g[i as usize])))
        .collect();
    let counter = Params::new(Kind::Counter, Some(15 * SECOND));
    let c = [100.0, 104.5, 3.0, 2.5, 2.0, 7.25];
    let c: Vec<_> = [0, 10, 20, 40, 45, 50]
        .iter()
        .zip(c)
        .map(|(s, v)| at(*s, Value::F64(v)))
        .collect();
    let wrap = Params {
        counter: CounterRules {
            wrap_at: Some(65_536.0),
            reset_ceiling: 0.1,
        },
        ..counter
    };
    let w: Vec<_> = [65_000.0, 65_530.0, 12.0, 40.0]
        .iter()
        .enumerate()
        .map(|(i, v)| at(i as i64 * 15, Value::F64(*v)))
        .collect();
    let state = Params::new(Kind::State, None);
    let s: Vec<_> = [0u16, 0, 3, 65_535, 0]
        .iter()
        .enumerate()
        .map(|(i, v)| at(i as i64 * 11, Value::State(*v)))
        .collect();
    let b: Vec<_> = [true, false, false, true]
        .iter()
        .enumerate()
        .map(|(i, v)| at(i as i64 * 14, Value::Bool(*v)))
        .collect();
    let text = Params::new(Kind::Text, Some(30 * SECOND));
    let t = vec![
        at(1, Value::Text(String::new())),
        at(30, Value::Text("door: öppen".into())),
    ];
    let interval = Params::interval(5 * MINUTE);
    let iv: Vec<_> = [0.25, 1.5, 0.75]
        .iter()
        .enumerate()
        .map(|(i, v)| (T0 + i as i64 * 5 * MINUTE, Value::F64(*v)))
        .collect();
    let quarter: Vec<Node> = (0..15)
        .map(|m| {
            let r: Vec<_> = (0..12)
                .map(|k| {
                    let s = m * 60 + k * 5;
                    at(s, Value::F64(20.0 + (s % 17) as f64 / 16.0))
                })
                .collect();
            fold(
                &Params::new(Kind::Gauge, Some(15 * SECOND)),
                T0 + m * MINUTE,
                T0 + (m + 1) * MINUTE,
                &r,
            )
        })
        .collect();
    vec![
        ("gauge-empty", Node::empty(Kind::Gauge, T0, T0 + MINUTE)),
        ("gauge", minute(&gauge, &g)),
        ("gauge-one", minute(&gauge, &g[..1])),
        ("counter", minute(&counter, &c)),
        ("counter-wrap", minute(&wrap, &w)),
        ("state", minute(&state, &s)),
        ("state-bool", minute(&state, &b)),
        ("text", minute(&text, &t)),
        ("interval", fold(&interval, T0, T0 + 15 * MINUTE, &iv)),
        (
            "gauge-quarter",
            merge_all(&Params::new(Kind::Gauge, Some(15 * SECOND)), T0, &quarter),
        ),
    ]
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

#[test]
fn every_golden_node_is_what_the_encoder_writes_and_decodes_to_its_node() {
    let bless = std::env::var_os("KVIK_BLESS_GOLDEN").is_some();
    for (name, node) in nodes() {
        let path = dir().join(format!("{name}.node"));
        let bytes = encode(&node).unwrap();
        if bless {
            fs::write(&path, &bytes).unwrap();
        }
        let file = fs::read(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(file, bytes, "{name}: the encoder writes other bytes");
        assert_eq!(decode(&file).unwrap(), node, "{name}");
    }
}

#[test]
fn an_unknown_form_is_a_version_refusal() {
    let path = dir().join("unknown-form.node");
    if std::env::var_os("KVIK_BLESS_GOLDEN").is_some() {
        let mut b = encode(&nodes()[1].1).unwrap();
        b[0] = 0x11;
        fs::write(&path, b).unwrap();
    }
    assert_eq!(
        decode(&fs::read(path).unwrap()),
        Err(DecodeError::UnsupportedForm(0x11))
    );
}
