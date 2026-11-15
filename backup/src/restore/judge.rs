//! When a listed file is lost in the bucket, and when it is tried again (Q-82). Pure: the caller passes `now`.

use std::time::Duration;

use super::RestoreConfig;

/// What to do with a file after a failed try.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Try it again, not before `now + after`.
    Retry(Duration),
    /// The Q-82 thresholds are met.
    Lost,
}

/// One file's tries in this run; a restart starts them again (Q-82).
#[derive(Clone, Debug, Default)]
pub(crate) struct FileState {
    /// No try before this.
    pub(crate) not_before: Duration,
    /// Why the last try failed.
    pub(crate) reason: String,
    failures: u32,
    absent: u32,
    first_absent: Option<Duration>,
    bad_downloads: u32,
}

fn backoff(cfg: &RestoreConfig, failures: u32) -> Duration {
    let factor = 1u32
        .checked_shl(failures.saturating_sub(1))
        .unwrap_or(u32::MAX);
    cfg.retry_first.saturating_mul(factor).min(cfg.retry_max)
}

impl FileState {
    fn retry(&mut self, cfg: &RestoreConfig, now: Duration, reason: String) -> Verdict {
        self.failures = self.failures.saturating_add(1);
        self.reason = reason;
        let after = backoff(cfg, self.failures);
        self.not_before = now.saturating_add(after);
        Verdict::Retry(after)
    }

    /// A fault that says nothing about the object: retried, never counted.
    pub(crate) fn transient(
        &mut self,
        cfg: &RestoreConfig,
        now: Duration,
        reason: String,
    ) -> Verdict {
        self.retry(cfg, now, reason)
    }

    /// `stat` found the object absent while the store descriptor answered: lost after `lost_tries` such tries
    /// spanning at least `lost_over`.
    pub(crate) fn absent(&mut self, cfg: &RestoreConfig, now: Duration) -> Verdict {
        self.absent = self.absent.saturating_add(1);
        let first = *self.first_absent.get_or_insert(now);
        let elapsed = now.saturating_sub(first);
        if self.absent >= cfg.lost_tries && elapsed >= cfg.lost_over {
            return Verdict::Lost;
        }
        let v = self.retry(cfg, now, "the bucket reports the object absent".into());
        if self.absent >= cfg.lost_tries {
            let rest = cfg.lost_over.saturating_sub(elapsed);
            self.not_before = now.saturating_add(rest);
            return Verdict::Retry(rest);
        }
        v
    }

    /// The object is present again: absent tries start again.
    pub(crate) fn present(&mut self) {
        self.absent = 0;
        self.first_absent = None;
    }

    /// A download that fails with the right key (its bytes, its authentication or its plaintext hash): lost after
    /// `lost_tries` such downloads.
    pub(crate) fn bad_download(
        &mut self,
        cfg: &RestoreConfig,
        now: Duration,
        reason: String,
    ) -> Verdict {
        self.bad_downloads = self.bad_downloads.saturating_add(1);
        if self.bad_downloads >= cfg.lost_tries {
            return Verdict::Lost;
        }
        self.retry(cfg, now, reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanMode;

    const MIN: Duration = Duration::from_secs(60);

    fn cfg() -> RestoreConfig {
        RestoreConfig {
            mode: PlanMode::Full,
            lost_tries: 3,
            lost_over: 10 * MIN,
            retry_first: Duration::from_secs(1),
            retry_max: Duration::from_secs(120),
        }
    }

    #[test]
    fn backoff_doubles_up_to_its_ceiling() {
        let c = cfg();
        let got: Vec<u64> = (1..=10).map(|n| backoff(&c, n).as_secs()).collect();
        assert_eq!(got, [1, 2, 4, 8, 16, 32, 64, 120, 120, 120]);
        assert_eq!(backoff(&c, u32::MAX), c.retry_max);
    }

    #[test]
    fn three_absent_tries_within_ten_minutes_are_not_a_loss() {
        let (c, mut s) = (cfg(), FileState::default());
        assert!(matches!(s.absent(&c, Duration::ZERO), Verdict::Retry(_)));
        assert!(matches!(s.absent(&c, MIN), Verdict::Retry(_)));
        assert_eq!(s.absent(&c, 2 * MIN), Verdict::Retry(8 * MIN));
        assert_eq!(s.not_before, 10 * MIN);
        assert_eq!(s.absent(&c, 10 * MIN), Verdict::Lost);
    }

    #[test]
    fn two_absent_tries_over_ten_minutes_are_not_a_loss() {
        let (c, mut s) = (cfg(), FileState::default());
        s.absent(&c, Duration::ZERO);
        assert!(matches!(s.absent(&c, 20 * MIN), Verdict::Retry(_)));
        assert_eq!(s.absent(&c, 21 * MIN), Verdict::Lost);
    }

    #[test]
    fn an_object_seen_again_starts_its_absent_tries_again() {
        let (c, mut s) = (cfg(), FileState::default());
        s.absent(&c, Duration::ZERO);
        s.absent(&c, MIN);
        s.present();
        s.absent(&c, 11 * MIN);
        s.absent(&c, 12 * MIN);
        assert!(matches!(s.absent(&c, 13 * MIN), Verdict::Retry(_)));
        assert_eq!(s.absent(&c, 21 * MIN), Verdict::Lost);
    }

    #[test]
    fn transient_faults_never_make_a_loss() {
        let (c, mut s) = (cfg(), FileState::default());
        for n in 0..1000u64 {
            assert!(matches!(
                s.transient(&c, Duration::from_secs(n * 600), "x".into()),
                Verdict::Retry(_)
            ));
        }
    }

    #[test]
    fn the_third_bad_download_is_a_loss() {
        let (c, mut s) = (cfg(), FileState::default());
        assert!(matches!(
            s.bad_download(&c, Duration::ZERO, "x".into()),
            Verdict::Retry(_)
        ));
        assert!(matches!(
            s.bad_download(&c, Duration::ZERO, "x".into()),
            Verdict::Retry(_)
        ));
        assert_eq!(
            s.bad_download(&c, Duration::ZERO, "x".into()),
            Verdict::Lost
        );
    }
}
