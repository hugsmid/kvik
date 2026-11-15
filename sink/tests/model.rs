//! Under transient faults the sink keeps the rules of a create-if-absent store
//! with no overwrite, modelled by a map.

use std::collections::BTreeMap;
use std::sync::Arc;

use kvik_sink::block_on;
use kvik_sink::fake::{FakeBucket, FakeOp, Fault};
use kvik_sink::verify::sha256;
use kvik_sink_api::{BackupSink, SinkError};
use proptest::prelude::*;

const KEYS: [&str; 4] = ["claims/00000001", "claims/00000002", "log/a", "log/b"];
const CONTENTS: [&[u8]; 3] = [b"", b"first", b"second"];

fn fault() -> impl Strategy<Value = Option<(FakeOp, Fault)>> {
    prop_oneof![
        3 => Just(None),
        1 => Just(Some((FakeOp::Put, Fault::TimeoutAfter))),
        1 => Just(Some((FakeOp::Put, Fault::TimeoutBefore))),
        1 => Just(Some((FakeOp::Put, Fault::CutUpload))),
        1 => Just(Some((FakeOp::Put, Fault::Status(503)))),
        1 => Just(Some((FakeOp::Head, Fault::Status(500)))),
        1 => Just(Some((FakeOp::Head, Fault::TimeoutAfter))),
    ]
}

proptest! {
    #[test]
    fn puts_under_faults_follow_the_model(
        ops in prop::collection::vec((0..KEYS.len(), 0..CONTENTS.len(), fault(), fault()), 1..12)
    ) {
        let bucket = FakeBucket::new();
        let sink = bucket.sink();
        let mut model: BTreeMap<&str, &[u8]> = BTreeMap::new();
        for (k, c, f1, f2) in ops {
            let put_fault = [f1, f2].iter().flatten().any(|(op, _)| *op == FakeOp::Put);
            for (op, f) in [f1, f2].into_iter().flatten() {
                bucket.inject(op, f);
            }
            let (key, content) = (KEYS[k], CONTENTS[c]);
            let got = block_on(sink.put(key, Arc::from(content), sha256(content)));
            match model.get(key) {
                // After an upload of ours may have landed, other content
                // reads as our upload stored damaged.
                Some(existing) if *existing != content => {
                    let expected = if put_fault {
                        SinkError::HashMismatch { key: key.into() }
                    } else {
                        SinkError::Exists { key: key.into() }
                    };
                    prop_assert_eq!(got, Err(expected));
                }
                _ => {
                    let stored = got.unwrap();
                    prop_assert_eq!(stored.size, content.len() as u64);
                    prop_assert_eq!(stored.sha256, sha256(content));
                    model.insert(key, content);
                }
            }
            bucket.clear_faults();
        }
        for (key, content) in &model {
            prop_assert_eq!(bucket.object(key), Some(content.to_vec()));
            prop_assert_eq!(block_on(sink.get(key, None)).unwrap(), content.to_vec());
        }
        let page = block_on(sink.list("", None)).unwrap();
        let expected: Vec<String> = model.keys().map(|k| k.to_string()).collect();
        prop_assert_eq!(page.keys, expected);
    }
}
