//! Fault trials on a real file system (gate 4.9; Q-52 (b) and Q-86 (b) in `model/OPEN-QUESTIONS.md`), driven by
//! `scripts/trial-rewrite.sh` and `scripts/trial-no-space.sh`.
//!
//! ```text
//! cargo build --release --locked -p kvik-log --example fault_trial
//! fault_trial probe <file>
//! fault_trial write <store> <acks> [--pause-after N] [--batches M] [--per-call K] [--segment-size BYTES]
//!                                  [--disk-critical BYTES] [--timeout SECS] [--boot-id HEX32]
//! fault_trial start <store> [--boot-id HEX32]
//! fault_trial verify <store> <acks> [--boot-id HEX32] [--allow-recorded-losses]
//! fault_trial damage <store>
//! ```
//!
//! Every line printed is `<command> key=value ...`, with any free text last, for the scripts to parse. Where a
//! command waits, it reads one line from standard input; a closed input ends it with exit status 5.
//!
//! - `probe`: the bare platform fact, without the engine. Writes 1 MiB of `0x5a` and syncs it; waits; writes 4 KiB
//!   of `0xa5` at the offset it printed and syncs (the script has failed the file's sectors: the sync must fail);
//!   waits; syncs again without writing (the control arm); waits; writes the same 4 KiB again and syncs (the
//!   treatment arm); waits.
//! - `write`: commits calls on [`StdFs`] with the mark synced at every commit, appends each acknowledged position
//!   to `<acks>` (a file on another file system) and syncs it. After `--pause-after` acknowledgements it prints the
//!   files in use and waits. It stops at the first fault and prints which call failed and the error kind; nothing
//!   is acknowledged after it. Exit status: 0 done, 3 a fault, 4 a call not acknowledged within `--timeout`
//!   (the reasons are printed), 1 any other error.
//! - `start`: one start of the writer; prints the start report and any fault. Exit status 0, 3 if the start left
//!   the store read-only, 1 if it could not open the store.
//! - `verify`: reads the store back through a reader process and checks that every position in `<acks>` is there.
//!   With `--allow-recorded-losses`, a missing position inside a lost range recorded in the manifest is counted but
//!   not a failure. Exit status 0 when nothing is missing.
//! - `damage`: overwrites the block header of the last segment in use's second last complete batch, so that the
//!   next start finds corruption below the mark and records a lost range (`spec.md` §3 Recovery rule 2).
//!
//! Each start after a remount should be given a new `--boot-id`: a remount under the same identity is a power loss
//! the engine is not told about.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::{Arc, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kvik_log::batch::SegmentIdentity;
use kvik_log::content::Item;
use kvik_log::facts::{FactFlags, FactRow, Quality, Value};
use kvik_log::fs::{Fs, StdFs};
use kvik_log::mark::read_mark;
use kvik_log::name::SegmentName;
use kvik_log::reader::{LogReader, ReadEvent, ReaderConfig};
use kvik_log::scan::{ScanLimit, scan};
use kvik_log::store::{MARK, NewStore, StoreDir, read_descriptor};
use kvik_log::writer::{Clock, Fault, Log, LogWriter, MarkSync, SystemClock, WriterConfig};

const STORE_ID: [u8; 16] = [0x4b; 16];
const BOOT: [u8; 16] = [0x01; 16];
const PROBE_LEN: usize = 1 << 20;
const PROBE_AT: u64 = 64 << 10;
const PROBE_PIECE: usize = 4096;
const FILL: u8 = 0x5a;
const PATTERN: u8 = 0xa5;

const EXIT_ERROR: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_FAULT: i32 = 3;
const EXIT_STALLED: i32 = 4;
const EXIT_STDIN: i32 = 5;

fn usage() -> ! {
    eprintln!(
        "usage: fault_trial probe <file>\n\
         \x20      fault_trial write <store> <acks> [--pause-after N] [--batches M] [--per-call K] \
         [--segment-size BYTES] [--disk-critical BYTES] [--timeout SECS] [--boot-id HEX32]\n\
         \x20      fault_trial start <store> [--boot-id HEX32]\n\
         \x20      fault_trial verify <store> <acks> [--boot-id HEX32] [--allow-recorded-losses]\n\
         \x20      fault_trial damage <store>"
    );
    exit(EXIT_USAGE)
}

fn fail(cmd: &str, what: &str, e: impl std::fmt::Display) -> ! {
    println!("{cmd} error={what} msg={e}");
    exit(EXIT_ERROR)
}

/// Waits for one line on standard input.
fn wait_line(cmd: &str) {
    let mut s = String::new();
    match io::stdin().lock().read_line(&mut s) {
        Ok(0) | Err(_) => {
            println!("{cmd} error=stdin-closed");
            exit(EXIT_STDIN)
        }
        Ok(_) => {}
    }
}

fn wall_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

fn reading(n: u64) -> Item {
    Item::Fact(FactRow {
        series_id: 1,
        t: wall_ns(),
        quality: Quality::Good,
        flags: FactFlags::default(),
        value: Value::F64((n as f64 * 0.25).to_bits()),
        changed_at: None,
    })
}

/// Options after the positional arguments.
#[derive(Debug)]
struct Opts {
    pause_after: Option<u64>,
    batches: u64,
    per_call: usize,
    segment_size: u64,
    disk_critical: Option<u64>,
    timeout: Duration,
    boot_id: [u8; 16],
    allow_recorded_losses: bool,
}

fn parse_boot(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

fn parse_opts(args: &[String]) -> Opts {
    let mut o = Opts {
        pause_after: None,
        batches: 100,
        per_call: 1,
        segment_size: WriterConfig::default().segment_size,
        disk_critical: None,
        timeout: Duration::from_secs(60),
        boot_id: BOOT,
        allow_recorded_losses: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut num = || -> u64 {
            it.next()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| usage())
        };
        match a.as_str() {
            "--pause-after" => o.pause_after = Some(num()),
            "--batches" => o.batches = num(),
            "--per-call" => o.per_call = usize::try_from(num()).unwrap_or_else(|_| usage()),
            "--segment-size" => o.segment_size = num(),
            "--disk-critical" => o.disk_critical = Some(num()),
            "--timeout" => o.timeout = Duration::from_secs(num()),
            "--boot-id" => {
                o.boot_id = it
                    .next()
                    .and_then(|v| parse_boot(v))
                    .unwrap_or_else(|| usage());
            }
            "--allow-recorded-losses" => o.allow_recorded_losses = true,
            _ => usage(),
        }
    }
    o
}

fn config(o: &Opts) -> WriterConfig {
    let mut c = WriterConfig {
        segment_size: o.segment_size,
        mark_sync: MarkSync::EveryCommit,
        ..WriterConfig::default()
    };
    if let Some(d) = o.disk_critical {
        c.disk_critical = d;
    }
    c
}

fn result_line(r: &io::Result<()>) -> String {
    match r {
        Ok(()) => "result=ok".into(),
        Err(e) => format!("result=err kind={:?} msg={e}", e.kind()),
    }
}

// ----- probe -----

fn probe(file: &Path) {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(file)
        .unwrap_or_else(|e| fail("probe", "open", e));
    let write_at = |at: u64, bytes: &[u8]| -> io::Result<()> {
        let mut h = &f;
        h.seek(SeekFrom::Start(at))?;
        h.write_all(bytes)
    };
    let r = write_at(0, &vec![FILL; PROBE_LEN]).and_then(|()| f.sync_all());
    println!("probe step=create {}", result_line(&r));
    if r.is_err() {
        exit(EXIT_ERROR);
    }
    println!(
        "probe step=ready offset={PROBE_AT} len={PROBE_PIECE} fill=0x{FILL:02x} pattern=0x{PATTERN:02x}"
    );
    wait_line("probe");
    let piece = vec![PATTERN; PROBE_PIECE];
    let r = write_at(PROBE_AT, &piece);
    println!("probe step=fault-write {}", result_line(&r));
    let r = f.sync_all();
    println!("probe step=fault-sync {}", result_line(&r));
    wait_line("probe");
    let r = f.sync_all();
    println!("probe step=control-sync {}", result_line(&r));
    wait_line("probe");
    let r = write_at(PROBE_AT, &piece);
    println!("probe step=treatment-write {}", result_line(&r));
    let r = f.sync_all();
    println!("probe step=treatment-sync {}", result_line(&r));
    wait_line("probe");
    println!("probe step=done");
}

// ----- write -----

fn fs() -> Arc<dyn Fs> {
    Arc::new(StdFs)
}

fn segment_path(store: &Path, file_id: u64) -> PathBuf {
    store.join("log").join(
        SegmentName {
            generation: 0,
            file_id,
            sealed: false,
        }
        .to_string(),
    )
}

/// The last complete batch of an unsealed segment, read through the file system (the page cache).
fn print_last_batch(store: &Path, file_id: Option<u64>) {
    let Some(file_id) = file_id else {
        println!("last_batch error=no-segment-in-use");
        return;
    };
    let path = segment_path(store, file_id);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            println!(
                "last_batch file={} error=read kind={:?} msg={e}",
                path.display(),
                e.kind()
            );
            return;
        }
    };
    let id = SegmentIdentity {
        store_id: STORE_ID,
        file_id,
        generation: 0,
    };
    match scan(&bytes, &id, ScanLimit::End) {
        Ok(r) => match r.batches.last() {
            Some(b) => println!(
                "last_batch file={} offset={} len={} first={} last={}",
                path.display(),
                b.offset,
                b.end() - b.offset,
                b.commit.first_sequence,
                b.commit.last_sequence
            ),
            None => println!("last_batch file={} error=no-batch", path.display()),
        },
        Err(e) => println!("last_batch file={} error=scan msg={e}", path.display()),
    }
}

fn print_fault(cmd: &str, f: &Fault) {
    println!(
        "{cmd} fault op={:?} kind={:?} path={} msg={}",
        f.op,
        f.kind,
        f.path.display(),
        f.message
    );
}

fn write(store_dir: &Path, acks_path: &Path, o: &Opts) {
    let new = NewStore {
        store_id: STORE_ID,
        created_at: wall_ns(),
        site_zone: "Etc/UTC".into(),
        tzdata: "2026a".into(),
        software: "kvik-log fault_trial".into(),
    };
    let store =
        StoreDir::open(fs(), store_dir, Some(&new)).unwrap_or_else(|e| fail("write", "store", e));
    let clock = SystemClock::default();
    let w = LogWriter::open(
        store,
        config(o),
        Some(o.boot_id),
        clock.monotonic(),
        clock.wall(),
    )
    .unwrap_or_else(|e| fail("write", "open", e));
    if let Some(f) = w.fault() {
        print_fault("write", f);
        exit(EXIT_FAULT);
    }
    println!("write start rule={:?}", w.start_report().rule);
    let log = Log::start(w, Arc::new(clock)).unwrap_or_else(|e| fail("write", "threads", e));
    let mut acks = OpenOptions::new()
        .create(true)
        .append(true)
        .open(acks_path)
        .unwrap_or_else(|e| fail("write", "acks", e));
    let mut acked = 0u64;
    for n in 0..o.batches {
        if o.pause_after == Some(acked) {
            let open = log.stats().open_file_id;
            println!(
                "write paused acked={acked} file={} mark={} manifest={}",
                open.map_or_else(String::new, |id| segment_path(store_dir, id)
                    .display()
                    .to_string()),
                store_dir.join("log").join(MARK).display(),
                store_dir.join("MANIFEST").display()
            );
            print_last_batch(store_dir, open);
            wait_line("write");
        }
        let items = (0..o.per_call)
            .map(|i| reading(n * o.per_call as u64 + i as u64))
            .collect();
        let pending = match log.submit(items) {
            Ok(p) => p,
            Err(e) => match log.fault() {
                Some(f) => stop_at_fault(&log, store_dir, &f, acked),
                None => fail("write", "submit", e),
            },
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(pending.wait());
        });
        match rx.recv_timeout(o.timeout) {
            Ok(Ok(ack)) => {
                let mut lines = String::new();
                for s in ack.first.sequence..=ack.last.sequence {
                    lines.push_str(&format!("{s}\n"));
                }
                acks.write_all(lines.as_bytes())
                    .and_then(|()| acks.sync_data())
                    .unwrap_or_else(|e| fail("write", "acks", e));
                acked += 1;
            }
            Ok(Err(e)) => match log.fault() {
                Some(f) => stop_at_fault(&log, store_dir, &f, acked),
                None => fail("write", "commit", e),
            },
            Err(_) => {
                // No spaces in the value, for the scripts.
                let reasons = format!("{:?}", log.reasons()).replace(' ', "");
                println!(
                    "write stalled acked={acked} waited_s={} reasons={reasons}",
                    o.timeout.as_secs()
                );
                exit(EXIT_STALLED);
            }
        }
    }
    let open = log.stats().open_file_id;
    print_last_batch(store_dir, open);
    if let Err(f) = log.shutdown() {
        print_fault("write", &f);
        exit(EXIT_FAULT);
    }
    println!("write done acked={acked}");
}

fn stop_at_fault(log: &Log, store: &Path, f: &Fault, acked: u64) -> ! {
    print_fault("write", f);
    println!("write stopped acked={acked}");
    print_last_batch(store, log.stats().open_file_id);
    exit(EXIT_FAULT)
}

// ----- start -----

fn start(store_dir: &Path, o: &Opts) {
    let store = StoreDir::open(fs(), store_dir, None).unwrap_or_else(|e| fail("start", "store", e));
    let clock = SystemClock::default();
    let mut w = LogWriter::open(
        store,
        config(o),
        Some(o.boot_id),
        clock.monotonic(),
        clock.wall(),
    )
    .unwrap_or_else(|e| fail("start", "open", e));
    let r = w.start_report();
    println!(
        "start rule={:?} rewrote_last_batch={} truncated_bytes={} losses={} entry={} deleted={:?} to_seal={:?} \
         headers_rewritten={:?}",
        r.rule,
        r.rewrote_last_batch,
        r.truncated_bytes,
        r.losses.len(),
        r.entry.is_some(),
        r.deleted,
        r.to_seal,
        r.headers_rewritten
    );
    for l in &r.losses {
        println!(
            "start loss kind={:?} cause={:?} first={:?} last={:?} file_id={} bytes={} readable_commits={} \
             recorded={}",
            l.kind, l.cause, l.first, l.last, l.file_id, l.bytes, l.readable_commits, l.recorded
        );
    }
    if let Some(f) = w.fault() {
        print_fault("start", f);
        println!("start readonly=true");
        exit(EXIT_FAULT);
    }
    let now = SystemClock::default().monotonic();
    if let Err(f) = w.shutdown(now) {
        print_fault("start", &f);
        exit(EXIT_FAULT);
    }
    println!("start readonly=false");
}

// ----- verify -----

fn read_acks(path: &Path) -> BTreeSet<u64> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| fail("verify", "acks", e));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            l.trim()
                .parse()
                .unwrap_or_else(|e| fail("verify", "acks", e))
        })
        .collect()
}

/// Positions read back, and the lost ranges the manifest records (first and last position).
type ReadBack = (BTreeSet<u64>, Vec<(u64, u64)>);

/// Every position the reader returns, and the lost ranges the manifest records.
fn read_with_reader(store: &Path, boot: [u8; 16]) -> Result<ReadBack, String> {
    let mut reader_id = [0u8; 16];
    reader_id[..8].copy_from_slice(&wall_ns().to_le_bytes());
    reader_id[8..].copy_from_slice(&std::process::id().to_le_bytes().repeat(2));
    let cfg = ReaderConfig::new(
        store,
        fs(),
        Arc::new(SystemClock::default()),
        Some(boot),
        reader_id,
    );
    let mut r = LogReader::open(cfg).map_err(|e| e.to_string())?;
    let mut have = BTreeSet::new();
    for _ in 0..1_000_000 {
        let u = r.refresh().map_err(|e| e.to_string())?;
        if u.events.is_empty() {
            break;
        }
        for ev in u.events {
            match ev {
                ReadEvent::Batch(b) => have.extend(b.first.sequence..=b.last.sequence),
                ReadEvent::Void { above, cause } => {
                    println!("verify void above={} cause={cause:?}", above.sequence);
                    have.retain(|&s| s <= above.sequence);
                }
                ReadEvent::Corruption { file_id, region } => println!(
                    "verify corruption file_id={file_id} start={} end={}",
                    region.start, region.end
                ),
            }
        }
    }
    let lost = r
        .lost_ranges()
        .iter()
        .map(|l| (l.first.1, l.last.1))
        .collect();
    let _ = r.close();
    Ok((have, lost))
}

/// The positions of the complete batches up to the mark, read from the segment files without a reader process:
/// used only where a reader cannot open (it writes a lease file, which a full disk may refuse).
fn read_by_scan(store: &Path) -> Result<BTreeSet<u64>, String> {
    let d = read_descriptor(&StdFs, store).map_err(|e| e.to_string())?;
    let mark = std::fs::read(store.join("log").join(MARK)).map_err(|e| e.to_string())?;
    let Some((_, slot)) = read_mark(&mark, &d.store_id).map_err(|e| e.to_string())? else {
        return Err("no valid mark slot".into());
    };
    let mut names: Vec<SegmentName> = std::fs::read_dir(store.join("log"))
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .filter_map(|e| SegmentName::parse(&e.file_name().to_string_lossy()))
        .filter(|n| n.file_id <= slot.file_id)
        .collect();
    names.sort_by_key(|n| n.file_id);
    let mut have = BTreeSet::new();
    for n in names {
        let bytes =
            std::fs::read(store.join("log").join(n.to_string())).map_err(|e| e.to_string())?;
        let id = SegmentIdentity {
            store_id: d.store_id,
            file_id: n.file_id,
            generation: n.generation,
        };
        let limit = if n.file_id == slot.file_id {
            ScanLimit::Mark(slot.offset)
        } else {
            ScanLimit::End
        };
        let r = scan(&bytes, &id, limit).map_err(|e| e.to_string())?;
        for b in r.batches {
            if b.commit.last_sequence <= slot.last_sequence {
                have.extend(b.commit.first_sequence..=b.commit.last_sequence);
            }
        }
    }
    Ok(have)
}

fn verify(store: &Path, acks_path: &Path, o: &Opts) {
    let acks = read_acks(acks_path);
    let (have, lost, via) = match read_with_reader(store, o.boot_id) {
        Ok((h, l)) => (h, l, "reader"),
        Err(e) => {
            println!("verify reader=unavailable msg={e}");
            let h = read_by_scan(store).unwrap_or_else(|e| fail("verify", "scan", e));
            (h, Vec::new(), "scan")
        }
    };
    let missing: Vec<u64> = acks.difference(&have).copied().collect();
    let in_lost = |s: u64| lost.iter().any(|&(a, b)| (a..=b).contains(&s));
    let excused: Vec<u64> = missing
        .iter()
        .copied()
        .filter(|&s| o.allow_recorded_losses && in_lost(s))
        .collect();
    let bad: Vec<u64> = missing
        .iter()
        .copied()
        .filter(|s| !excused.contains(s))
        .collect();
    println!(
        "verify via={via} acked={} read={} missing={} excused={} lost_ranges={lost:?}",
        acks.len(),
        have.len(),
        bad.len(),
        excused.len()
    );
    if !bad.is_empty() {
        let shown: Vec<u64> = bad.iter().copied().take(20).collect();
        println!("verify missing_positions={shown:?}");
        exit(EXIT_ERROR);
    }
    println!("verify ok=true");
}

// ----- damage -----

fn damage(store: &Path) {
    let d = read_descriptor(&StdFs, store).unwrap_or_else(|e| fail("damage", "store", e));
    let mut open: Vec<SegmentName> = std::fs::read_dir(store.join("log"))
        .unwrap_or_else(|e| fail("damage", "list", e))
        .filter_map(|e| e.ok())
        .filter_map(|e| SegmentName::parse(&e.file_name().to_string_lossy()))
        .filter(|n| !n.sealed)
        .collect();
    open.sort_by_key(|n| n.file_id);
    for n in open.iter().rev() {
        let path = store.join("log").join(n.to_string());
        let bytes = std::fs::read(&path).unwrap_or_else(|e| fail("damage", "read", e));
        let id = SegmentIdentity {
            store_id: d.store_id,
            file_id: n.file_id,
            generation: n.generation,
        };
        let r = scan(&bytes, &id, ScanLimit::End).unwrap_or_else(|e| fail("damage", "scan", e));
        if r.batches.len() < 2 {
            continue;
        }
        let b = &r.batches[r.batches.len() - 2];
        let f = OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap_or_else(|e| fail("damage", "open", e));
        let mut h = &f;
        h.seek(SeekFrom::Start(b.offset))
            .and_then(|_| h.write_all(&[0xff; 16]))
            .and_then(|()| f.sync_all())
            .unwrap_or_else(|e| fail("damage", "write", e));
        println!(
            "damage file={} offset={} first={} last={}",
            path.display(),
            b.offset,
            b.commit.first_sequence,
            b.commit.last_sequence
        );
        return;
    }
    fail(
        "damage",
        "no-segment",
        "no unsealed segment holds two complete batches",
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = args.split_first().unwrap_or_else(|| usage());
    match (cmd.as_str(), rest) {
        ("probe", [file]) => probe(Path::new(file)),
        ("write", [store, acks, opts @ ..]) => {
            write(Path::new(store), Path::new(acks), &parse_opts(opts));
        }
        ("start", [store, opts @ ..]) => start(Path::new(store), &parse_opts(opts)),
        ("verify", [store, acks, opts @ ..]) => {
            verify(Path::new(store), Path::new(acks), &parse_opts(opts));
        }
        ("damage", [store]) => damage(Path::new(store)),
        _ => usage(),
    }
}
