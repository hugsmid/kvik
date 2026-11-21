//! The read-only command-line tool `kvik` (the query design's section 6): `series`, `latest` (static and `--live`),
//! `tail`, `health` and `info` on a store directory. It opens a [`kvik::Reader`] and never a store: in the store
//! directory it writes only its lease, under `leases/` (Q-727), released when it ends.
//!
//! ```no_run
//! let args: Vec<String> = ["latest", "--store", "store", "*/*/power"].map(String::from).into();
//! kvik_cli::run(&args, None, kvik::env::Env::system(), &mut std::io::stdout()).unwrap();
//! ```

#![forbid(unsafe_code)]

mod args;
mod commands;
mod files;
mod live;
mod out;
mod show;

use std::io::Write;
use std::path::PathBuf;

pub use args::{Args, Command, Format, USAGE};

/// Why a command failed.
#[derive(Debug)]
pub enum Error {
    /// The command line is wrong.
    Usage(String),
    /// The store could not be opened for reading.
    Open(kvik::OpenError),
    /// A read failed.
    Query(kvik::QueryError),
    /// Writing the output failed.
    Output(std::io::Error),
    /// The answer was written, but the reader knows of lost data it may lack (the query design's section 6).
    Losses(Vec<String>),
    /// A live read was closed; its row was written.
    Closed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Usage(s) => write!(f, "{s}"),
            Error::Open(e) => write!(f, "cannot read the store: {e}"),
            Error::Query(e) => write!(f, "{e}"),
            Error::Output(e) => write!(f, "output: {e}"),
            Error::Losses(l) => write!(
                f,
                "the answer may be incomplete: {} loss{} ({}); see `kvik health`",
                l.len(),
                if l.len() == 1 { "" } else { "es" },
                l.join(", ")
            ),
            Error::Closed(r) => write!(f, "the live read was closed: {r}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<kvik::QueryError> for Error {
    fn from(e: kvik::QueryError) -> Self {
        Error::Query(e)
    }
}

impl From<kvik::OpenError> for Error {
    fn from(e: kvik::OpenError) -> Self {
        Error::Open(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Output(e)
    }
}

/// Runs the command line `args` (without the program name) in `env`, writing to `out`; `store` is the directory used
/// when `--store` is not given.
pub fn run(
    args: &[String],
    store: Option<PathBuf>,
    env: kvik::env::Env,
    out: &mut dyn Write,
) -> Result<(), Error> {
    let a = Args::parse(args, store)?;
    let clock = show::Clock::new(a.zone.as_deref())?;
    let reader = kvik::Reader::open(kvik::ReaderConfig::new(&a.store).env(env.clone()))?;
    let c = commands::Ctx {
        args: &a,
        reader: &reader,
        env: &env,
        clock: &clock,
    };
    let done = match a.command {
        Command::Series => commands::series(&c, out),
        Command::Latest { live: false } => commands::latest(&c, out),
        Command::Latest { live: true } => live::latest(&c, out),
        Command::Tail => live::tail(&c, out),
        Command::Health => commands::health(&c, out),
        Command::Info => commands::info(&c, out),
    };
    out.flush()?;
    done
}
