//! The command line (the query design's section 6; Q-1050, Q-1051).

use std::path::PathBuf;
use std::time::Duration;

use crate::Error;

/// Output format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    /// Aligned columns.
    #[default]
    Table,
    /// Comma-separated values with a header line.
    Csv,
    /// One JSON object a line.
    Jsonl,
}

/// A command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `kvik series [pattern…]`.
    Series,
    /// `kvik latest [pattern…] [--live]`.
    Latest {
        /// Then updates as they become durable.
        live: bool,
    },
    /// `kvik tail [pattern…]`.
    Tail,
    /// `kvik health`.
    Health,
    /// `kvik info`.
    Info,
}

/// A parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    /// The command.
    pub command: Command,
    /// The store directory.
    pub store: PathBuf,
    /// Series name patterns: three `/`-separated segments, each a literal or `*`; none selects every series.
    pub patterns: Vec<String>,
    /// Output format.
    pub format: Format,
    /// The zone times are shown in; UTC when `None`.
    pub zone: Option<String>,
    /// How often a live command looks for new data (default 200 ms, at least 20 ms).
    pub interval: Duration,
    /// A live command stops after this long; it runs until stopped when `None`.
    pub run_for: Option<Duration>,
}

/// The usage text.
pub const USAGE: &str = "\
usage: kvik <command> --store <dir> [options] [pattern...]

commands:
  series [pattern...]          names, ids, kinds, units and latest times
  latest [pattern...] [--live] the live-state fact of each series, one cut; --live: then its updates
  tail [pattern...]            facts as they become durable, with resets, gaps and lag
  health                       losses, position, segments, leases, derived files, the writer's status
  info                         store descriptor, zone and tzdata, files by kind

options:
  --store <dir>                the store directory (or KVIK_STORE)
  --format table|csv|jsonl     output format (default table)
  --zone <zone>                show times in this zone (default UTC)
  --interval <duration>        live commands: how often to look for new data (default 200ms)
  --for <duration>             live commands: stop after this long
";

fn usage(what: impl Into<String>) -> Error {
    Error::Usage(what.into())
}

/// A duration such as `200ms`, `10s`, `10m` or `1h`.
pub fn duration(s: &str) -> Result<Duration, Error> {
    let at = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| usage(format!("duration {s:?} needs a unit (ms, s, m, h)")))?;
    let (n, unit) = s.split_at(at);
    let n: u64 = n
        .parse()
        .map_err(|_| usage(format!("duration {s:?} is not a number")))?;
    let ms = match unit {
        "ms" => Some(n),
        "s" => n.checked_mul(1_000),
        "m" => n.checked_mul(60_000),
        "h" => n.checked_mul(3_600_000),
        _ => return Err(usage(format!("duration {s:?} has an unknown unit"))),
    };
    ms.map(Duration::from_millis)
        .ok_or_else(|| usage(format!("duration {s:?} is too long")))
}

impl Args {
    /// Parses `args` (without the program name); `store` is the directory used when `--store` is not given.
    pub fn parse(args: &[String], store: Option<PathBuf>) -> Result<Args, Error> {
        let mut it = args.iter();
        let command = match it.next().map(String::as_str) {
            Some("series") => Command::Series,
            Some("latest") => Command::Latest { live: false },
            Some("tail") => Command::Tail,
            Some("health") => Command::Health,
            Some("info") => Command::Info,
            Some(other) => return Err(usage(format!("unknown command {other:?}"))),
            None => return Err(usage("no command")),
        };
        let mut a = Args {
            command,
            store: PathBuf::new(),
            patterns: Vec::new(),
            format: Format::Table,
            zone: None,
            interval: Duration::from_millis(200),
            run_for: None,
        };
        let mut store = store;
        while let Some(arg) = it.next() {
            let mut value = |name: &str| {
                it.next()
                    .cloned()
                    .ok_or_else(|| usage(format!("{name} needs a value")))
            };
            match arg.as_str() {
                "--store" => store = Some(PathBuf::from(value("--store")?)),
                "--format" => {
                    a.format = match value("--format")?.as_str() {
                        "table" => Format::Table,
                        "csv" => Format::Csv,
                        "jsonl" => Format::Jsonl,
                        other => return Err(usage(format!("unknown format {other:?}"))),
                    }
                }
                "--zone" => a.zone = Some(value("--zone")?),
                "--interval" => a.interval = duration(&value("--interval")?)?,
                "--for" => a.run_for = Some(duration(&value("--for")?)?),
                "--live" => match &mut a.command {
                    Command::Latest { live } => *live = true,
                    _ => return Err(usage("--live is an option of latest")),
                },
                // Q-1050: a tail from a time waits for the as-of time of roadmap 9.0f (Q-1010).
                "--since" | "--once" => {
                    return Err(usage(format!(
                        "{arg} is not available yet: a tail opens at the newest durable position"
                    )));
                }
                s if s.starts_with("--") => return Err(usage(format!("unknown option {s:?}"))),
                p => a.patterns.push(p.to_owned()),
            }
        }
        a.store = store.ok_or_else(|| usage("no store directory: give --store or set KVIK_STORE"))?;
        let live = matches!(a.command, Command::Tail | Command::Latest { live: true });
        if !live && a.run_for.is_some() {
            return Err(usage("--for is an option of the live commands"));
        }
        if a.interval < Duration::from_millis(20) {
            return Err(usage("--interval must be at least 20ms"));
        }
        if matches!(a.command, Command::Health | Command::Info) && !a.patterns.is_empty() {
            return Err(usage("health and info take no pattern"));
        }
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Args, Error> {
        let v: Vec<String> = s.split_whitespace().map(String::from).collect();
        Args::parse(&v, None)
    }

    #[test]
    fn durations_take_a_unit_and_refuse_an_overflow() {
        assert_eq!(duration("200ms").unwrap(), Duration::from_millis(200));
        assert_eq!(duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(duration("1h").unwrap(), Duration::from_secs(3_600));
        for bad in ["10", "ms", "5d", "99999999999999999999h"] {
            assert!(duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_command_line_parses_its_options_and_patterns() {
        let a = parse("latest --store s --live --format csv a/*/c --interval 50ms --for 2s").unwrap();
        assert_eq!(a.command, Command::Latest { live: true });
        assert_eq!(a.patterns, ["a/*/c"]);
        assert_eq!(a.format, Format::Csv);
        assert_eq!(a.interval, Duration::from_millis(50));
        assert_eq!(a.run_for, Some(Duration::from_secs(2)));
        let k = Args::parse(&["info".into()], Some("k".into())).unwrap();
        assert_eq!(k.store, PathBuf::from("k"));
    }

    #[test]
    fn options_out_of_place_are_refused() {
        for bad in [
            "series",
            "series --store s --live",
            "health --store s x/y/z",
            "series --store s --for 1s",
            "tail --store s --since 10m",
            "tail --store s --interval 10ms",
            "latest --store s --format xml",
            "frob --store s",
        ] {
            assert!(matches!(parse(bad), Err(Error::Usage(_))), "{bad}");
        }
    }
}
