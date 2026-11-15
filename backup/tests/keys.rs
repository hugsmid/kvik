//! Object keys (`format.md` A.17 Keys, Q-198 (e), Q-271): every built key is short ASCII, parses back to what it
//! names under its own store only, and a generation's records list in epoch order.

use kvik_backup::keys::{Keys, ObjectName};
use kvik_files::desc::{FileClass, FileDescription, FileRef, kind};
use proptest::prelude::*;

fn desc(kind: u8, generation: u32, file_id: u64) -> FileDescription {
    FileDescription {
        kind,
        class: FileClass::None,
        generation,
        file_id,
        range: None,
        hash: [0; 32],
        inputs: Vec::new(),
    }
}

fn named(k: &Keys, kd: u8, g: u32, f: u64, epoch: u64) -> Vec<(String, ObjectName)> {
    vec![
        (k.store(), ObjectName::Store),
        (k.probe(), ObjectName::Probe),
        (
            k.file(&desc(kd, g, f)),
            ObjectName::File {
                file: FileRef {
                    kind: kd,
                    file_id: f,
                },
                generation: g,
            },
        ),
        (
            k.file(&desc(kind::LOG_SEGMENT, g, f)),
            ObjectName::File {
                file: FileRef {
                    kind: kind::LOG_SEGMENT,
                    file_id: f,
                },
                generation: g,
            },
        ),
        (k.claim(g), ObjectName::Claim(g)),
        (
            k.record(g, epoch),
            ObjectName::Record {
                generation: g,
                epoch,
            },
        ),
    ]
}

proptest! {
    #[test]
    fn every_built_key_is_short_ascii_and_parses_back_under_its_own_store_only(id in any::<[u8; 16]>(), other in any::<[u8; 16]>(), kd in any::<u8>(), g in any::<u32>(), f in any::<u64>(), epoch in any::<u64>()) {
        let k = Keys::new(id);
        let mut o = other;
        if o == id { o[0] ^= 1; }
        let ko = Keys::new(o);
        for (key, name) in named(&k, kd, g, f, epoch) {
            prop_assert!(key.is_ascii() && key.len() <= 128, "{key}");
            prop_assert!(!key.bytes().any(|b| b.is_ascii_uppercase()) || key.ends_with("/STORE"), "{key}");
            prop_assert_eq!(k.parse(&key), Some(name), "{}", key);
            prop_assert_eq!(Keys::store_id_of(&key), Some(id));
            prop_assert_eq!(ko.parse(&key), None);
        }
    }

    #[test]
    fn records_of_a_generation_list_in_epoch_order(id in any::<[u8; 16]>(), g in any::<u32>(), epochs in proptest::collection::vec(any::<u64>(), 1..20)) {
        let k = Keys::new(id);
        let mut keys: Vec<String> = epochs.iter().map(|&e| k.record(g, e)).collect();
        keys.sort();
        let listed: Vec<u64> = keys.iter().map(|s| match k.parse(s) {
            Some(ObjectName::Record { epoch, .. }) => epoch,
            other => panic!("{other:?}"),
        }).collect();
        let mut want = epochs.clone();
        want.sort();
        prop_assert_eq!(listed, want);
        prop_assert!(keys.iter().all(|s| s.starts_with(&k.records_prefix(g))));
    }

    #[test]
    fn parsing_any_string_never_panics_and_only_accepts_canonical_keys(id in any::<[u8; 16]>(), tail in "[0-9a-fA-F/.\\-a-z]{0,48}") {
        let k = Keys::new(id);
        let root = &k.store()[..33];
        let key = format!("{root}{tail}");
        if let Some(name) = k.parse(&key) {
            let rebuilt = match name {
                ObjectName::Store => k.store(),
                ObjectName::Probe => k.probe(),
                ObjectName::File { file, generation } => k.file(&desc(file.kind, generation, file.file_id)),
                ObjectName::Record { generation, epoch } => k.record(generation, epoch),
                ObjectName::Claim(g) => k.claim(g),
                other => panic!("{other:?}"),
            };
            prop_assert_eq!(rebuilt, key);
        }
        let _ = Keys::store_id_of(&tail);
    }
}
