//! `kvik`: the read-only command-line tool (the query design's section 6).

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        None | Some("-h" | "--help" | "help")
    ) {
        print!("{}", kvik_cli::USAGE);
        return ExitCode::SUCCESS;
    }
    lower_priority();
    let store = std::env::var_os("KVIK_STORE").map(Into::into);
    let mut out = std::io::stdout().lock();
    match kvik_cli::run(&args, store, kvik::env::Env::system(), &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e @ kvik_cli::Error::Usage(_)) => {
            eprintln!("kvik: {e}\n\n{}", kvik_cli::USAGE);
            ExitCode::from(2)
        }
        Err(e @ kvik_cli::Error::Query(kvik::QueryError::Invalid { .. })) => {
            eprintln!("kvik: {e}");
            ExitCode::from(2)
        }
        // A reader that stopped reading (`kvik latest | head`) is not a failure.
        Err(kvik_cli::Error::Output(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("kvik: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A heavy query like any other (the query design's section 3.4) runs at lowered priority; a refusal changes nothing read.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
fn lower_priority() {
    let _ = rustix::process::setpriority_process(None, 10);
}

/// No priority call where rustix has none (Windows).
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
fn lower_priority() {}
