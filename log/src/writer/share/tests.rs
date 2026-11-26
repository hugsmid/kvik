use proptest::prelude::*;

use super::*;

const MS: Duration = Duration::from_millis(1);

/// The writer as the simulation sees it: a sync in flight always, returning every `every` (never at `None`).
struct Syncs {
    every: Option<Duration>,
    pressure: bool,
}

impl Syncs {
    fn state(&self, now: Duration) -> WriterState {
        WriterState {
            syncing: true,
            syncs_returned: self
                .every
                .map_or(0, |e| (now.as_nanos() / e.as_nanos()) as u64),
            pressure: self.pressure,
        }
    }

    fn next_return(&self, now: Duration) -> Option<Duration> {
        self.every.map(|e| {
            let k = now.as_nanos() / e.as_nanos() + 1;
            Duration::from_nanos((k * e.as_nanos()) as u64)
        })
    }
}

/// Runs a task whose every step works `work` and issues `piece` for `secs`; returns (CPU worked, bytes, the
/// longest gap between device calls).
fn simulate(
    cfg: ShareConfig,
    syncs: &Syncs,
    work: Duration,
    piece: Option<u64>,
    secs: u64,
) -> (Duration, u64, Duration) {
    let end = Duration::from_secs(secs);
    let mut g = Governor::new(cfg, Duration::ZERO);
    let mut now = Duration::ZERO;
    let mut last_call = Duration::ZERO;
    let mut gap = Duration::ZERO;
    while now < end {
        let w = syncs.state(now);
        match g.admit(now, piece, &w) {
            Verdict::Go => {
                let start = now;
                now += work;
                g.worked(start, now, piece, &syncs.state(now));
                if piece.is_some() {
                    gap = gap.max(start - last_call);
                    last_call = start;
                }
            }
            Verdict::Wait { until, on_sync } => {
                assert!(until > now, "a wait that does not move time");
                let woken = syncs.next_return(now).filter(|_| on_sync);
                now = woken.map_or(until, |r| r.min(until));
            }
        }
    }
    let (worked, bytes) = g.used();
    (worked, bytes, gap)
}

fn rate(bytes: u64, secs: u64) -> f64 {
    bytes as f64 / secs as f64
}

#[test]
fn the_share_holds_and_is_never_zero_under_one_sync_that_never_returns() {
    let cfg = ShareConfig::for_cores(4);
    let syncs = Syncs {
        every: None,
        pressure: false,
    };
    let (worked, bytes, gap) = simulate(cfg, &syncs, MS, Some(PIECE), 60);
    let floor = cfg.floor_rate() as f64;
    assert!(rate(bytes, 60) >= 0.95 * floor && rate(bytes, 60) <= 1.05 * floor);
    assert!(gap <= Duration::from_secs_f64(PIECE as f64 / floor) + MS);
    assert!(worked.as_secs_f64() / 60.0 <= cfg.cpu_share);
}

#[test]
fn the_share_holds_and_is_never_zero_under_back_to_back_syncs() {
    let cfg = ShareConfig::for_cores(1);
    for every in [MS, 4 * MS, 40 * MS] {
        let syncs = Syncs {
            every: Some(every),
            pressure: false,
        };
        let (worked, bytes, _) = simulate(cfg, &syncs, MS, Some(PIECE), 30);
        assert!(rate(bytes, 30) >= 0.95 * cfg.floor_rate() as f64);
        assert!(rate(bytes, 30) <= cfg.io_rate as f64 + PIECE as f64);
        assert!(worked.as_secs_f64() / 30.0 <= cfg.cpu_share + 1e-9);
    }
}

#[test]
fn an_idle_writer_gives_the_task_its_io_rate_and_its_cpu_share() {
    let cfg = ShareConfig::for_cores(4);
    let idle = Syncs {
        every: None,
        pressure: false,
    };
    let idle_state = |g: &mut Governor, now| g.admit(now, Some(PIECE), &WriterState::default());
    let mut g = Governor::new(cfg, Duration::ZERO);
    assert_eq!(idle_state(&mut g, Duration::ZERO), Verdict::Go);
    let (_, bytes, _) = {
        let mut g = Governor::new(cfg, Duration::ZERO);
        let mut now = Duration::ZERO;
        while now < Duration::from_secs(20) {
            match g.admit(now, Some(PIECE), &WriterState::default()) {
                Verdict::Go => {
                    let s = now;
                    now += Duration::from_micros(10);
                    g.worked(s, now, Some(PIECE), &WriterState::default());
                }
                Verdict::Wait { until, on_sync } => {
                    assert!(!on_sync);
                    now = until;
                }
            }
        }
        (g.used().0, g.used().1, ())
    };
    assert!((rate(bytes, 20) - cfg.io_rate as f64).abs() <= 0.05 * cfg.io_rate as f64);
    let (worked, _, _) = simulate(cfg, &idle, 20 * MS, None, 20);
    assert!((worked.as_secs_f64() / 20.0 - cfg.cpu_share).abs() < 0.01);
}

#[test]
fn under_pressure_the_task_runs_at_the_floor_rate_and_half_its_cpu_share() {
    let cfg = ShareConfig {
        cpu_share: 0.25,
        io_rate: 32 << 20,
    };
    let syncs = Syncs {
        every: Some(MS),
        pressure: true,
    };
    let (_, bytes, _) = simulate(cfg, &syncs, Duration::from_micros(1), Some(PIECE), 20);
    let floor = cfg.floor_rate() as f64;
    assert!(rate(bytes, 20) <= floor + PIECE as f64 && rate(bytes, 20) >= 0.9 * floor);
    let (worked, _, _) = simulate(cfg, &syncs, 10 * MS, None, 20);
    assert!((worked.as_secs_f64() / 20.0 - 0.125).abs() < 0.01);
}

#[test]
fn a_returning_sync_lets_a_piece_go_before_its_deadline() {
    let cfg = ShareConfig::for_cores(4);
    let mut g = Governor::new(cfg, Duration::ZERO);
    let busy = |n| WriterState {
        syncing: true,
        syncs_returned: n,
        pressure: false,
    };
    assert!(matches!(
        g.admit(Duration::ZERO, Some(PIECE), &busy(0)),
        Verdict::Wait { on_sync: true, .. }
    ));
    assert_eq!(
        g.admit(Duration::ZERO, Some(PIECE), &WriterState::default()),
        Verdict::Go
    );
    g.worked(Duration::ZERO, MS, Some(PIECE), &busy(0));
    let t = Duration::from_millis(50);
    assert!(matches!(
        g.admit(t, Some(PIECE), &busy(0)),
        Verdict::Wait { on_sync: true, .. }
    ));
    assert_eq!(g.admit(t + MS, Some(PIECE), &busy(1)), Verdict::Go);
    assert_eq!(
        g.admit(t + MS, Some(0), &WriterState::default()),
        Verdict::Go
    );
}

#[test]
fn the_floor_rate_is_an_eighth_of_the_io_rate_and_at_least_128_kib_a_second() {
    let c = |io_rate| ShareConfig {
        cpu_share: 0.25,
        io_rate,
    };
    assert_eq!(c(8 << 20).floor_rate(), 1 << 20);
    assert_eq!(c(1 << 20).floor_rate(), 128 * 1024);
    assert_eq!(ShareConfig::for_cores(1).cpu_share, 0.15);
}

proptest! {
    #[test]
    fn neither_limit_is_ever_passed_and_the_floor_is_always_met(
        share in 0.05f64..1.0,
        io_mib in 1u64..64,
        work_us in 1u64..20_000,
        every_ms in proptest::option::of(1u64..100),
        pressure in any::<bool>(),
        piece in proptest::option::of(0u64..=PIECE),
    ) {
        let cfg = ShareConfig { cpu_share: share, io_rate: io_mib << 20 };
        let syncs = Syncs { every: every_ms.map(Duration::from_millis), pressure };
        let secs = 10;
        let (worked, bytes, _) = simulate(cfg, &syncs, Duration::from_micros(work_us), piece, secs);
        let s = if pressure { share / 2.0 } else { share };
        // One step may finish past the end.
        let slack = Duration::from_micros(work_us).as_secs_f64();
        prop_assert!(worked.as_secs_f64() <= s * secs as f64 + slack);
        let r = if pressure { cfg.floor_rate() } else { cfg.io_rate };
        prop_assert!(bytes <= r * secs + PIECE);
        if piece == Some(PIECE) {
            // At least the floor, unless the CPU share allows fewer steps.
            let cpu_bound = s / Duration::from_micros(work_us).as_secs_f64() * PIECE as f64;
            let want = (cfg.floor_rate() as f64).min(cpu_bound) * secs as f64;
            prop_assert!(bytes as f64 >= 0.85 * want - PIECE as f64, "{bytes} < {want}");
        }
    }
}
