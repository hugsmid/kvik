//! The tool writes only under `leases/` (the query design's section 6, Q-727), with the writer running and with it
//! shut, and leaves no lease behind.

mod common;

use common::Fixture;

const COMMANDS: &[&str] = &[
    "series",
    "latest",
    "latest --live --for 0ms",
    "tail --for 100ms",
    "health",
    "info --format jsonl",
];

fn only_leases_change(f: &Fixture) {
    for cmd in COMMANDS {
        let before = f.files();
        f.run(cmd).unwrap();
        let after = f.files();
        let leases = f.dir.join("leases");
        let changed: Vec<_> = before
            .iter()
            .filter(|b| !after.contains(b))
            .chain(after.iter().filter(|a| !before.contains(a)))
            .map(|(p, _)| p.clone())
            .filter(|p| !p.starts_with(&leases))
            .collect();
        assert!(changed.is_empty(), "{cmd}: {changed:?}");
        let held = |v: &[(std::path::PathBuf, Vec<u8>)]| {
            v.iter()
                .filter(|(p, _)| p.starts_with(&leases))
                .map(|(p, _)| p.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(held(&before), held(&after), "{cmd}: its lease is left");
    }
}

#[test]
fn the_tool_writes_only_under_leases_with_the_writer_running_and_shut() {
    let mut f = Fixture::new();
    only_leases_change(&f);
    f.shut();
    only_leases_change(&f);
}

#[test]
fn the_tool_writes_its_lease_while_it_runs() {
    let f = Fixture::new();
    let leases = f.dir.join("leases");
    let count = |f: &Fixture| f.files().iter().filter(|(p, _)| p.starts_with(&leases)).count();
    let before = count(&f);
    let out = f.run("health").unwrap();
    assert!(
        out.lines().any(|l| l.starts_with("leases") && l.trim_end().ends_with(&(before + 1).to_string())),
        "{out}"
    );
    assert_eq!(count(&f), before);
}
