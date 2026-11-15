//! One hand-built vector per check of `docs/format-tree.md` T.6, each refused for that check alone.

use kvik_tree::{DecodeError, Kind, Params, Value, decode, encode, fold, form};

/// A node's bytes, field by field (T.2–T.5).
#[derive(Default)]
struct B(Vec<u8>);

impl B {
    fn head(form: u8, start: i64, width: u64, count: u64, coverage: u128) -> B {
        let mut b = B::default();
        b.0.push(form);
        b.0.extend_from_slice(&start.to_le_bytes());
        b.uv(u128::from(width)).uv(u128::from(count)).uv(coverage)
    }
    fn uv(mut self, mut v: u128) -> B {
        loop {
            let byte = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(byte);
                return self;
            }
            self.0.push(byte | 0x80);
        }
    }
    fn u8(mut self, v: u8) -> B {
        self.0.push(v);
        self
    }
    fn f64(mut self, v: f64) -> B {
        self.0.extend_from_slice(&v.to_bits().to_le_bytes());
        self
    }
    fn zero(self) -> B {
        self.u8(0)
    }
    /// A small positive odd integer as an exact number.
    fn int(self, m: u8) -> B {
        self.u8(1).u8(0).u8(0).uv(1).u8(m)
    }
}

fn refused(b: B, what: &str) {
    match decode(&b.0) {
        Err(DecodeError::Invalid { what: w, .. }) => assert_eq!(w, what),
        other => panic!("expected {what:?}, got {other:?}"),
    }
}

#[test]
fn the_builder_writes_what_the_encoder_writes() {
    let p = Params::new(Kind::Gauge, None);
    let n = fold(&p, 0, 100, &[(0, Value::F64(1.0)), (10, Value::F64(1.0))]);
    let b = B::head(form::GAUGE, 0, 100, 2, 10)
        .uv(0)
        .f64(1.0)
        .uv(10)
        .f64(1.0)
        .u8(1)
        .u8(1)
        .u8(0)
        .uv(1)
        .u8(1)
        .u8(1)
        .u8(1)
        .u8(0)
        .uv(1)
        .u8(5)
        .f64(1.0)
        .f64(1.0);
    assert_eq!(encode(&n).unwrap(), b.0);
    assert_eq!(decode(&b.0).unwrap(), n);
}

#[test]
fn an_integer_out_of_range() {
    let mut b = B::head(form::TEXT, 0, 100, 0, 0);
    b.0.truncate(9);
    let b = (0..10).fold(b, |b, _| b.u8(0xFF)).u8(0x01);
    refused(b, "integer out of range");
}

#[test]
fn an_end_beyond_the_time_range() {
    refused(
        B::head(form::TEXT, i64::MAX, 1, 0, 0),
        "span end beyond the time range",
    );
}

#[test]
fn coverage_without_readings() {
    refused(
        B::head(form::TEXT, 0, 100, 0, 5),
        "coverage without readings",
    );
}

#[test]
fn more_readings_than_nanoseconds_between_first_and_last() {
    let b = B::head(form::TEXT, 0, 100, 3, 0).uv(0).u8(0).uv(1).u8(0);
    refused(b, "more readings than nanoseconds between first and last");
}

#[test]
fn a_value_that_is_not_finite() {
    let b = B::head(form::GAUGE, 0, 100, 1, 0).uv(0).f64(f64::NAN);
    refused(b, "a value that is not finite");
}

#[test]
fn a_sum_without_readings() {
    let b = B::head(form::GAUGE, 0, 100, 0, 0).int(1).zero();
    refused(b, "sum without readings");
}

#[test]
fn an_integral_without_coverage() {
    let b = B::head(form::GAUGE, 0, 100, 1, 0)
        .uv(0)
        .f64(1.0)
        .int(1)
        .int(1);
    refused(b, "integral without coverage");
}

#[test]
fn min_and_max_that_differ_with_one_reading() {
    let b = B::head(form::GAUGE, 0, 100, 1, 0)
        .uv(0)
        .f64(1.0)
        .int(1)
        .zero()
        .f64(1.0)
        .f64(2.0);
    refused(b, "min and max differ with one reading");
}

#[test]
fn an_increase_without_a_step() {
    let b = B::head(form::COUNTER, 0, 100, 1, 0)
        .uv(0)
        .f64(5.0)
        .int(1)
        .uv(0)
        .uv(0)
        .uv(0);
    refused(b, "increase without a step");
}

#[test]
fn more_decreases_or_long_steps_than_steps() {
    let two = || {
        B::head(form::COUNTER, 0, 100, 2, 0)
            .uv(0)
            .f64(5.0)
            .uv(10)
            .f64(1.0)
            .zero()
    };
    refused(
        two().uv(2).uv(0).uv(0),
        "more decreases or long steps than steps",
    );
    refused(
        two().uv(1).uv(1).uv(0),
        "more decreases or long steps than steps",
    );
    refused(
        two().uv(0).uv(0).uv(2),
        "more decreases or long steps than steps",
    );
}

#[test]
fn more_transitions_than_steps() {
    let b = B::head(form::STATE, 0, 100, 1, 0).uv(0).u8(0).uv(1).uv(0);
    refused(b, "more transitions than pairs");
}

#[test]
fn a_state_time_of_zero() {
    let b = B::head(form::STATE, 0, 100, 2, 0)
        .uv(0)
        .u8(0)
        .uv(10)
        .u8(1)
        .uv(1)
        .uv(1)
        .u8(0)
        .uv(0);
    refused(b, "a state time of zero");
}

#[test]
fn state_times_that_do_not_add_up_to_the_coverage() {
    let b = B::head(form::STATE, 0, 100, 2, 10)
        .uv(0)
        .u8(0)
        .uv(10)
        .u8(1)
        .uv(1)
        .uv(1)
        .u8(0)
        .uv(5);
    refused(b, "state times do not add up to the coverage");
}
