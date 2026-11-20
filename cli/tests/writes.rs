//! The tool writes only under `leases/` (the query design's section 6, Q-727): every call of its file system that
//! could change a file or take a lock names a path there, with the writer running and with it shut; with it shut,
//! no other file changes; and the tool leaves no lease behind.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{Fixture, Recording};

const COMMANDS: &[&str] = &[
    "series",
    "latest",
    "latest --live --for 0ms",
    "tail --for 100ms",
    "health",
    "info --format jsonl",
];

fn leases_of(f: &Fixture, v: &[(PathBuf, Vec<u8>)]) -> Vec<PathBuf> {
    let leases = f.dir.join("leases");
    v.iter()
        .filter(|(p, _)| p.starts_with(&leases))
        .map(|(p, _)| p.clone())
        .collect()
}

fn only_leases_written(f: &Fixture, writer_running: bool) {
    let leases = f.dir.join("leases");
    for cmd in COMMANDS {
        let rec = Recording::new(f.env.fs.clone());
        let mut env = f.env.clone();
        env.fs = Arc::new(rec.clone());
        let before = f.files();
        f.run_in(cmd, env).unwrap();
        let after = f.files();
        let log = rec.log.lock().unwrap().clone();
        assert!(log.iter().any(|(_, p)| p.starts_with(&leases)), "{cmd}: no lease written");
        let outside: Vec<_> = log.iter().filter(|(_, p)| !p.starts_with(&leases)).collect();
        assert!(outside.is_empty(), "{cmd}: {outside:?}");
        // The writer rewrites its own files while it runs (its status file, A.18).
        if !writer_running {
            let changed: Vec<_> = before
                .iter()
                .filter(|b| !after.contains(b))
                .chain(after.iter().filter(|a| !before.contains(a)))
                .map(|(p, _)| p.clone())
                .filter(|p| !p.starts_with(&leases))
                .collect();
            assert!(changed.is_empty(), "{cmd}: {changed:?}");
        }
        assert_eq!(leases_of(f, &before), leases_of(f, &after), "{cmd}: its lease is left");
    }
}

#[test]
fn the_tool_writes_only_under_leases_with_the_writer_running_and_shut() {
    let mut f = Fixture::new();
    only_leases_written(&f, true);
    f.shut();
    only_leases_written(&f, false);
}

#[test]
fn the_tool_holds_a_lease_while_it_runs() {
    let f = Fixture::new();
    let before = leases_of(&f, &f.files()).len();
    let out = f.run("health").unwrap();
    let held = out
        .lines()
        .find(|l| l.starts_with("leases"))
        .and_then(|l| l.split_whitespace().last())
        .unwrap();
    assert_eq!(held, (before + 1).to_string(), "{out}");
}
