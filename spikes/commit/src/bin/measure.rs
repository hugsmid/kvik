//! Measurements for roadmap item 2.2. Prints Markdown tables.
//!
//! ```text
//! cargo run --release -p commit --bin measure -- <dir> all [scale]
//! cargo run --release -p commit --bin measure -- <dir> load <per-series rate> <window ms> <seconds> [call] [mark]
//! cargo run --release -p commit --bin measure -- <dir> wa [seconds]
//! cargo run --release -p commit --bin measure -- <dir> sync [scale]
//! cargo run --release -p commit --bin measure -- <dir> calibrate
//! ```
//!
//! `<dir>` is a directory on the disk to measure; the runs work in a `kvik-commit-measure` subdirectory of it,
//! which is removed at the end. `scale` multiplies every duration (default 1; the full run takes about 13
//! minutes).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use commit::format::{self, Mark, PAYLOAD_BYTES_PER_READING, Reading};
use commit::fs::{FileSystem, RealFs, SyncCall, sync_dir};
use commit::stats::summary;
use commit::writer::{Config, MarkSync, Writer};

const SERIES: u64 = 100;

fn ms(ns: u64) -> String {
    format!("{:.2}", ns as f64 / 1e6)
}

fn us(ns: u64) -> String {
    format!("{:.1}", ns as f64 / 1e3)
}

fn fresh(dir: &Path, name: &str) -> PathBuf {
    let d = dir.join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("create scratch directory");
    d
}

/// Raw cost of each sync call after a small write: into a preallocated file, and appending to a growing one.
fn sync_calls(dir: &Path, scale: f64) {
    println!("\n### Sync calls after a 256-byte write, idle disk\n");
    println!("| Call | File | Syncs | p50 ms | p99 ms | max ms | syncs/s | Device KiB per sync |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|");
    let budget = Duration::from_secs_f64(2.0 * scale);
    for call in SyncCall::all() {
        for prealloc in [true, false] {
            let d = fresh(dir, "sync");
            let path = d.join("f");
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            if prealloc {
                use std::io::Write;
                let zeros = vec![0u8; 1 << 20];
                let mut f = &file;
                for _ in 0..64 {
                    f.write_all(&zeros).unwrap();
                }
                file.sync_all().unwrap();
            }
            sync_dir(&d).unwrap();
            let buf = [0xA5u8; 256];
            let mut times = Vec::new();
            let device_before = device_bytes_written();
            let start = Instant::now();
            let mut offset = 0u64;
            while start.elapsed() < budget && times.len() < 20_000 {
                write_at(&file, offset, &buf);
                offset += buf.len() as u64;
                let t = Instant::now();
                call.sync(&file).unwrap();
                times.push(t.elapsed().as_nanos() as u64);
            }
            let device = device_before
                .zip(device_bytes_written())
                .map(|(a, b)| b.saturating_sub(a));
            let total: u64 = times.iter().sum();
            let n = times.len();
            let [p50, p99, _, max] = summary(&mut times);
            println!(
                "| {} | {} | {n} | {} | {} | {} | {:.0} | {} |",
                call.name(),
                if prealloc { "preallocated" } else { "growing" },
                ms(p50),
                ms(p99),
                ms(max),
                n as f64 / (total as f64 / 1e9),
                device.map_or("–".into(), |d| format!(
                    "{:.1}",
                    d as f64 / 1024.0 / n as f64
                )),
            );
        }
    }
}

fn write_at(file: &std::fs::File, offset: u64, buf: &[u8]) {
    #[cfg(unix)]
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset).unwrap();
    #[cfg(not(unix))]
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = file;
        f.seek(SeekFrom::Start(offset)).unwrap();
        f.write_all(buf).unwrap();
    }
}

/// Segment preallocation and a seal-like sequence, the mark update, the mark sync and the free-space check.
fn fixed_costs(dir: &Path, call: SyncCall) {
    println!("\n### Fixed costs ({})\n", call.name());
    println!("| Operation | Repetitions | p50 | max |");
    println!("|---|---:|---:|---:|");
    let d = fresh(dir, "fixed");
    let fs = RealFs::new(&d, call).unwrap();
    let header = format::log_segment_header(1, 1, 1, 0, [0; 16]);
    let mut create = Vec::new();
    let mut seal = Vec::new();
    for i in 0..3 {
        let name = format!("log/1-{i:016x}.open");
        let t = Instant::now();
        drop(fs.create_preallocated(&name, 64 << 20, &header).unwrap());
        create.push(t.elapsed().as_nanos() as u64);
        // Seal-like: write a trailer, truncate, sync, rename, sync the directory.
        let path = d.join(&name);
        let t = Instant::now();
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        write_at(&f, 32 << 20, &[1u8; 96]);
        f.set_len((32 << 20) + 96).unwrap();
        // The length changed, so a data-only sync is not enough.
        f.sync_all().unwrap();
        std::fs::rename(&path, path.with_extension("kvl")).unwrap();
        sync_dir(&d.join("log")).unwrap();
        seal.push(t.elapsed().as_nanos() as u64);
    }
    let [p50, _, _, max] = summary(&mut create);
    println!(
        "| Create a 64 MiB zero-filled segment, sync, sync directory | 3 | {} ms | {} ms |",
        ms(p50),
        ms(max)
    );
    let [p50, _, _, max] = summary(&mut seal);
    println!(
        "| Seal-like: trailer, truncate, sync, rename, sync directory | 3 | {} ms | {} ms |",
        ms(p50),
        ms(max)
    );

    let mut mark = fs.open("log/MARK").unwrap();
    let m = Mark {
        seq: 0,
        boot_id: [0; 16],
        generation: 1,
        file_id: 1,
        offset: 64,
        last_sequence: 0,
    };
    mark.write_at(0, &m.encode()).unwrap();
    let mut updates = Vec::new();
    for i in 0..20_000u32 {
        let t = Instant::now();
        mark.write_at(12, &(2 * i + 1).to_le_bytes()).unwrap();
        mark.write_at(16, &m.encode()[16..]).unwrap();
        mark.write_at(12, &(2 * i + 2).to_le_bytes()).unwrap();
        updates.push(t.elapsed().as_nanos() as u64);
    }
    let [p50, _, _, max] = summary(&mut updates);
    println!(
        "| Mark update: three positional writes | 20,000 | {} µs | {} µs |",
        us(p50),
        us(max)
    );
    for c in SyncCall::all() {
        let mut syncs = Vec::new();
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(d.join("log/MARK"))
            .unwrap();
        for i in 0..100u32 {
            write_at(&f, 12, &(2 * i).to_le_bytes());
            let t = Instant::now();
            c.sync(&f).unwrap();
            syncs.push(t.elapsed().as_nanos() as u64);
        }
        let [p50, _, _, max] = summary(&mut syncs);
        println!(
            "| Mark sync with {} | 100 | {} ms | {} ms |",
            c.name(),
            ms(p50),
            ms(max)
        );
    }
    let mut checks = Vec::new();
    for _ in 0..10_000 {
        let t = Instant::now();
        fs.space().unwrap();
        checks.push(t.elapsed().as_nanos() as u64);
    }
    let [p50, _, _, max] = summary(&mut checks);
    println!(
        "| Free-space check (`statvfs`) | 10,000 | {} µs | {} µs |",
        us(p50),
        us(max)
    );
}

#[derive(Debug)]
struct LoadResult {
    offered: f64,
    readings: u64,
    achieved: f64,
    syncs_per_s: f64,
    per_batch: f64,
    ack: [u64; 4],
    delivered: [u64; 4],
    logical_wa: f64,
    page_wa: f64,
    mark_syncs: usize,
    mark_sync_max: u64,
    generator_lag_p99: u64,
    back_pressure_waits: u64,
    roll_wait_max: u64,
    rolls: u64,
    syncs: u64,
    log_bytes: u64,
    page_bytes: u64,
    device_bytes: Option<u64>,
}

/// Bytes the operating system reports written to block storage since boot, where it offers a counter without
/// privileges (macOS: `ioreg`). Includes every process; the idle baseline in `wa` shows the background rate.
fn device_bytes_written() -> Option<u64> {
    if !cfg!(target_vendor = "apple") {
        return None;
    }
    let out = std::process::Command::new("ioreg")
        .args(["-c", "IOBlockStorageDriver", "-r", "-w", "0"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut total = 0u64;
    for part in text.split("\"Bytes (Write)\"=").skip(1) {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        total += digits.parse::<u64>().ok()?;
    }
    Some(total)
}

/// One open-loop run: `SERIES` series at `rate` readings per second each, staggered evenly. `rate == 0` emits
/// as fast as back-pressure allows (closed loop) to find the ceiling.
fn load(
    dir: &Path,
    call: SyncCall,
    rate: u64,
    window: Duration,
    secs: f64,
    mark_sync: MarkSync,
) -> LoadResult {
    load_with(
        dir,
        call,
        rate,
        window,
        secs,
        Config {
            mark_sync,
            ..Config::default()
        },
    )
}

fn load_with(
    dir: &Path,
    call: SyncCall,
    rate: u64,
    window: Duration,
    secs: f64,
    config: Config,
) -> LoadResult {
    let d = fresh(dir, "load");
    let fs: Arc<dyn FileSystem> = Arc::new(RealFs::new(&d, call).unwrap());
    let expected = if rate == 0 {
        1_000_000
    } else {
        ((rate * SERIES) as f64 * secs) as usize
    };
    let latencies = Arc::new(Mutex::new(Vec::with_capacity(expected)));
    let batch_sizes = Arc::new(Mutex::new(Vec::new()));
    let (deliver, delivered) = mpsc::channel::<Vec<Instant>>();
    let consumer = thread::spawn(move || {
        let mut v = Vec::new();
        for batch in delivered {
            let now = Instant::now();
            v.extend(
                batch
                    .iter()
                    .map(|t| now.duration_since(*t).as_nanos() as u64),
            );
        }
        v
    });
    let (lat, sizes) = (Arc::clone(&latencies), Arc::clone(&batch_sizes));
    let config = Config {
        linger: window,
        ..config
    };
    let (writer, ingest) = Writer::start(
        fs,
        config,
        Box::new(move |c| {
            lat.lock().unwrap().extend(
                c.emitted
                    .iter()
                    .map(|t| c.acked_at.duration_since(*t).as_nanos() as u64),
            );
            sizes.lock().unwrap().push(c.emitted.len() as u64);
            let _ = deliver.send(c.emitted.to_vec());
        }),
    )
    .unwrap();

    // Let the background creation of the next segment finish first: it competes with the log sync (measured
    // separately under "Segment rolls").
    thread::sleep(Duration::from_millis(300));
    let device_before = device_bytes_written();
    let start = Instant::now();
    let mut lags = Vec::with_capacity(expected);
    let mut i: u64 = 0;
    let interval_ns = if rate == 0 {
        0
    } else {
        1_000_000_000 / (rate * SERIES)
    };
    let total = if rate == 0 { u64::MAX } else { expected as u64 };
    while i < total {
        let scheduled = start + Duration::from_nanos(i * interval_ns);
        if rate == 0 {
            if start.elapsed().as_secs_f64() >= secs {
                break;
            }
        } else {
            // Spin rather than sleep: a sleeping thread can wake milliseconds late, which would count as
            // commit latency. The generator occupies one core.
            while Instant::now() < scheduled {
                std::hint::spin_loop();
            }
        }
        let series = (i % SERIES) as u32;
        let r = Reading {
            series_id: series,
            t: i as i64,
            value: i as f64 * 0.25,
        };
        // Latency counts from the emit call. How late the generator was against its schedule is reported
        // separately; back-pressure, which would hide latency from an open-loop load, is counted and was zero.
        let emitted_at = Instant::now();
        ingest.emit(r, emitted_at).expect("emit refused");
        if rate != 0 {
            lags.push(emitted_at.duration_since(scheduled).as_nanos() as u64);
        }
        i += 1;
    }
    let (_, back_pressure_waits) = ingest.back_pressure();
    drop(ingest);
    let report = writer.join();
    let elapsed = start.elapsed().as_secs_f64();
    assert!(
        report.fatal.is_none(),
        "the writer failed: {:?}",
        report.fatal
    );
    let device_bytes = device_before
        .zip(device_bytes_written())
        .map(|(a, b)| b.saturating_sub(a));
    let mut delivered = consumer.join().unwrap();
    let mut ack = std::mem::take(&mut *latencies.lock().unwrap());
    let sizes = batch_sizes.lock().unwrap();
    // The report holds only the mark syncs made while running, not the clean-shutdown sync.
    let mark: Vec<u64> = report
        .mark_syncs
        .iter()
        .map(|d| d.as_nanos() as u64)
        .collect();
    let payload = report.readings * PAYLOAD_BYTES_PER_READING;
    let _ = std::fs::remove_dir_all(&d);
    LoadResult {
        offered: if rate == 0 {
            f64::NAN
        } else {
            (rate * SERIES) as f64
        },
        readings: report.readings,
        achieved: report.readings as f64 / elapsed,
        syncs_per_s: report.syncs as f64 / elapsed,
        per_batch: sizes.iter().sum::<u64>() as f64 / sizes.len().max(1) as f64,
        ack: summary(&mut ack),
        delivered: summary(&mut delivered),
        logical_wa: report.log_bytes as f64 / payload as f64,
        page_wa: (report.pages_touched * 4096) as f64 / payload as f64,
        mark_syncs: mark.len(),
        mark_sync_max: mark.iter().copied().max().unwrap_or(0),
        generator_lag_p99: summary(&mut lags)[1],
        back_pressure_waits,
        roll_wait_max: report
            .roll_waits
            .iter()
            .map(|d| d.as_nanos() as u64)
            .max()
            .unwrap_or(0),
        rolls: report.segments_rolled,
        syncs: report.syncs,
        log_bytes: report.log_bytes,
        page_bytes: report.pages_touched * 4096,
        device_bytes,
    }
}

/// How many times each cell of the matrix and of the mark table is run.
const REPS: usize = 3;

/// Median of `v`, with the lowest and highest in brackets.
fn spread(mut v: Vec<f64>, digits: usize) -> String {
    v.sort_by(f64::total_cmp);
    let median = v[v.len() / 2];
    format!(
        "{median:.digits$} ({:.digits$}–{:.digits$})",
        v[0],
        v[v.len() - 1]
    )
}

fn spread_ms(runs: &[LoadResult], f: impl Fn(&LoadResult) -> u64) -> String {
    spread(runs.iter().map(|r| f(r) as f64 / 1e6).collect(), 2)
}

fn header() {
    println!(
        "| Readings/s | Window ms | Readings | Syncs/s | Per batch | Ack p50 | p99 | p99.9 | max | Delivered p50 | p99 | p99.9 | Bytes/payload | Pages/payload |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
}

fn row(window: Duration, r: &LoadResult) {
    let offered = if r.offered.is_nan() {
        format!("max ({:.0})", r.achieved)
    } else {
        format!("{:.0}", r.offered)
    };
    println!(
        "| {offered} | {} | {} | {:.0} | {:.1} | {} | {} | {} | {} | {} | {} | {} | {:.2} | {:.1} |",
        window.as_millis(),
        r.readings,
        r.syncs_per_s,
        r.per_batch,
        ms(r.ack[0]),
        ms(r.ack[1]),
        ms(r.ack[2]),
        ms(r.ack[3]),
        ms(r.delivered[0]),
        ms(r.delivered[1]),
        ms(r.delivered[2]),
        r.logical_wa,
        r.page_wa,
    );
    eprintln!(
        "  achieved {:.0}/s, generator lag p99 {} µs, back-pressure waits {}, mark syncs {} (max {} ms), roll wait max {} ms",
        r.achieved,
        us(r.generator_lag_p99),
        r.back_pressure_waits,
        r.mark_syncs,
        ms(r.mark_sync_max),
        ms(r.roll_wait_max)
    );
}

fn secs_for(rate: u64, scale: f64) -> f64 {
    scale * if rate == 1 { 20.0 } else { 8.0 }
}

fn matrix(dir: &Path, scale: f64) {
    let call = SyncCall::durable();
    let mark = MarkSync::Inline(Duration::from_secs(1));
    // Warm-up, discarded.
    load(dir, call, 10, Duration::ZERO, 1.0 * scale, mark);
    println!(
        "\n### Group commit with {}, 100 series, mark synced every second on the writer thread\n",
        call.name()
    );
    println!("Each cell run {REPS} times: median (lowest–highest).\n");
    println!(
        "| Readings/s | Window ms | Syncs/s | Per batch | Delivered p50 ms | p99 ms | p99.9 ms | max ms | Bytes/payload | Pages/payload | Generator lag p99 µs, worst |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for rate in [1, 10, 100, 1000] {
        for window_ms in [0, 1, 5, 20] {
            let window = Duration::from_millis(window_ms);
            let runs: Vec<LoadResult> = (0..REPS)
                .map(|i| {
                    eprintln!("load {rate}/s/series window {window_ms} ms, run {}", i + 1);
                    let r = load(dir, call, rate, window, secs_for(rate, scale), mark);
                    assert_eq!(
                        r.back_pressure_waits, 0,
                        "an open-loop run met back-pressure"
                    );
                    r
                })
                .collect();
            println!(
                "| {} | {window_ms} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                rate * SERIES,
                spread(runs.iter().map(|r| r.syncs_per_s).collect(), 0),
                spread(runs.iter().map(|r| r.per_batch).collect(), 1),
                spread_ms(&runs, |r| r.delivered[0]),
                spread_ms(&runs, |r| r.delivered[1]),
                spread_ms(&runs, |r| r.delivered[2]),
                spread_ms(&runs, |r| r.delivered[3]),
                spread(runs.iter().map(|r| r.logical_wa).collect(), 2),
                spread(runs.iter().map(|r| r.page_wa).collect(), 1),
                us(runs.iter().map(|r| r.generator_lag_p99).max().unwrap_or(0)),
            );
            let ack_minus_delivered = runs
                .iter()
                .map(|r| r.delivered[0].saturating_sub(r.ack[0]))
                .max()
                .unwrap_or(0);
            eprintln!(
                "  delivered p50 minus ack p50, worst run: {} µs",
                us(ack_minus_delivered)
            );
        }
    }
    eprintln!("saturation");
    let r = load(dir, call, 0, Duration::ZERO, 5.0 * scale, mark);
    println!(
        "\nCeiling (emitting as fast as back-pressure allows, 1 MiB batches): {:.0} readings/s at {:.0} syncs/s, {:.0} readings per batch.",
        r.achieved, r.syncs_per_s, r.per_batch
    );
    let mark_thread = MarkSync::Thread(Duration::from_secs(1));
    println!(
        "\n### Segment rolls: 8 MiB segments at {} readings/s, window 0\n",
        1000 * SERIES
    );
    println!("| Next segment | Rolls | Roll wait max ms | Ack p50 | p99 | p99.9 | max |");
    println!("|---|---:|---:|---:|---:|---:|---:|");
    for ahead in [true, false] {
        eprintln!("rolls ahead={ahead}");
        let config = Config {
            segment_len: 8 << 20,
            preallocate_ahead: ahead,
            mark_sync: mark,
            ..Config::default()
        };
        let r = load_with(dir, call, 1000, Duration::ZERO, 12.0 * scale, config);
        println!(
            "| {} | {} | {} | {} | {} | {} | {} |",
            if ahead {
                "created ahead on another thread"
            } else {
                "created on the writer thread at the roll"
            },
            r.rolls,
            ms(r.roll_wait_max),
            ms(r.ack[0]),
            ms(r.ack[1]),
            ms(r.ack[2]),
            ms(r.ack[3])
        );
    }

    println!("\n### Other sync calls under load, window 0\n");
    println!("| Call | Readings/s | Syncs/s | Per batch | Ack p50 | p99 | p99.9 | max |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    for c in SyncCall::all() {
        if c == call {
            continue;
        }
        for rate in [10, 1000] {
            eprintln!("call {} {rate}/s/series", c.name());
            let r = load(dir, c, rate, Duration::ZERO, 8.0 * scale, mark);
            println!(
                "| {} | {:.0} | {:.0} | {:.1} | {} | {} | {} | {} |",
                c.name(),
                r.offered,
                r.syncs_per_s,
                r.per_batch,
                ms(r.ack[0]),
                ms(r.ack[1]),
                ms(r.ack[2]),
                ms(r.ack[3])
            );
        }
    }

    println!(
        "\n### Mark sync rule, {} readings/s, window 0, {}\n",
        10 * SERIES,
        call.name()
    );
    println!("Each rule run {REPS} times: median (lowest–highest).\n");
    println!(
        "| Mark sync | Syncs/s (log) | Mark syncs | Mark sync max ms | Ack p50 ms | p99 ms | p99.9 ms | max ms |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    let second = Duration::from_secs(1);
    for (name, m) in [
        ("off", MarkSync::Off),
        ("every 1 s, own thread", mark_thread),
        ("every 1 s, writer thread", MarkSync::Inline(second)),
        ("every batch", MarkSync::EveryBatch),
    ] {
        let runs: Vec<LoadResult> = (0..REPS)
            .map(|i| {
                eprintln!("mark {name}, run {}", i + 1);
                load(dir, call, 10, Duration::ZERO, 10.0 * scale, m)
            })
            .collect();
        println!(
            "| {name} | {} | {} | {} | {} | {} | {} | {} |",
            spread(runs.iter().map(|r| r.syncs_per_s).collect(), 0),
            spread(runs.iter().map(|r| r.mark_syncs as f64).collect(), 0),
            spread_ms(&runs, |r| r.mark_sync_max),
            spread_ms(&runs, |r| r.ack[0]),
            spread_ms(&runs, |r| r.ack[1]),
            spread_ms(&runs, |r| r.ack[2]),
            spread_ms(&runs, |r| r.ack[3]),
        );
    }
}

/// Write amplification: log bytes, page writes and (where available) device bytes per payload byte.
fn write_amplification(dir: &Path, secs: f64) {
    let call = SyncCall::durable();
    let before = device_bytes_written();
    thread::sleep(Duration::from_secs_f64(secs));
    let idle = before
        .zip(device_bytes_written())
        .map(|(a, b)| b.saturating_sub(a));
    println!(
        "\n### Write amplification, {}, window 0, {secs:.0} s per row\n",
        call.name()
    );
    match idle {
        Some(b) => println!(
            "Idle baseline: {:.1} KiB/s written to block storage by the whole machine.\n",
            b as f64 / 1024.0 / secs
        ),
        None => println!("No device counter on this operating system.\n"),
    }
    println!(
        "| Readings/s | Log syncs | Payload KiB | Log KiB | Pages KiB | Device KiB, less idle | Device KiB per sync | Log/payload | Pages/payload | Device/payload |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for rate in [1, 10, 1000] {
        eprintln!("wa {rate}/s/series");
        let r = load(
            dir,
            call,
            rate,
            Duration::ZERO,
            secs,
            MarkSync::Thread(Duration::from_secs(1)),
        );
        let payload = r.readings * PAYLOAD_BYTES_PER_READING;
        let device = r.device_bytes.zip(idle).map(|(d, i)| d.saturating_sub(i));
        let kib = |b: u64| format!("{:.0}", b as f64 / 1024.0);
        println!(
            "| {:.0} | {} | {} | {} | {} | {} | {} | {:.2} | {:.1} | {} |",
            r.offered,
            r.syncs,
            kib(payload),
            kib(r.log_bytes),
            kib(r.page_bytes),
            device.map_or("–".into(), kib),
            device.map_or("–".into(), |d| format!(
                "{:.1}",
                d as f64 / 1024.0 / r.syncs.max(1) as f64
            )),
            r.log_bytes as f64 / payload as f64,
            r.page_bytes as f64 / payload as f64,
            device.map_or("–".into(), |d| format!(
                "{:.1}",
                d as f64 / payload as f64
            )),
        );
    }
}

/// Checks the device counter against a known write: a 64 MiB file, written and synced.
fn calibrate(dir: &Path) {
    use std::io::Write;
    println!("\n### Device counter check\n");
    let Some(_) = device_bytes_written() else {
        println!("No device counter on this operating system.");
        return;
    };
    for i in 0..3 {
        let d = fresh(dir, "calibrate");
        let before = device_bytes_written().unwrap_or(0);
        let mut f = std::fs::File::create(d.join("f")).unwrap();
        let block = vec![0x5Au8; 1 << 20];
        for _ in 0..64 {
            f.write_all(&block).unwrap();
        }
        f.sync_all().unwrap();
        drop(f);
        let delta = device_bytes_written().unwrap_or(0).saturating_sub(before);
        println!(
            "Run {}: 64 MiB written and synced, counter moved by {:.1} MiB.",
            i + 1,
            delta as f64 / (1u64 << 20) as f64
        );
    }
}

fn mark_arg(s: &str) -> MarkSync {
    match s {
        "off" => MarkSync::Off,
        "inline" => MarkSync::Inline(Duration::from_secs(1)),
        "every" => MarkSync::EveryBatch,
        "thread" => MarkSync::Thread(Duration::from_secs(1)),
        _ => MarkSync::Inline(Duration::from_secs(1)),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: measure <dir> all [scale] | load <rate> <window ms> <seconds> [call] [mark] | wa [seconds] | sync [scale] | calibrate";
    let dir = PathBuf::from(args.get(1).expect(usage)).join("kvik-commit-measure");
    std::fs::create_dir_all(&dir).expect("create the scratch directory");
    match args.get(2).map(String::as_str) {
        Some("all") => {
            let scale: f64 = args.get(3).map_or(1.0, |s| s.parse().expect("scale"));
            println!(
                "Operating system: {}, durable call: {}",
                std::env::consts::OS,
                SyncCall::durable().name()
            );
            sync_calls(&dir, scale);
            fixed_costs(&dir, SyncCall::durable());
            matrix(&dir, scale);
        }
        Some("sync") => {
            let scale: f64 = args.get(3).map_or(1.0, |s| s.parse().expect("scale"));
            sync_calls(&dir, scale);
        }
        Some("calibrate") => calibrate(&dir),
        Some("wa") => {
            let secs: f64 = args.get(3).map_or(20.0, |s| s.parse().expect("seconds"));
            write_amplification(&dir, secs);
        }
        Some("load") => {
            let rate: u64 = args[3].parse().expect("rate");
            let window = Duration::from_millis(args[4].parse().expect("window"));
            let secs: f64 = args[5].parse().expect("seconds");
            let call = args
                .get(6)
                .map_or(SyncCall::durable(), |s| SyncCall::parse(s).expect("call"));
            let mark = args
                .get(7)
                .map_or(MarkSync::Inline(Duration::from_secs(1)), |s| mark_arg(s));
            header();
            row(window, &load(&dir, call, rate, window, secs, mark));
        }
        _ => panic!("{usage}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
