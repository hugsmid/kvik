//! `kvik`: the read-only command-line tool (the query design's section 6).

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), None | Some("-h" | "--help" | "help")) {
        print!("{}", kvik_cli::USAGE);
        return ExitCode::SUCCESS;
    }
    // A heavy query like any other (the query design's section 3.4): lowered priority. A refusal changes nothing read.
    let _ = rustix::process::setpriority_process(None, 10);
    let store = std::env::var_os("KVIK_STORE").map(Into::into);
    let mut out = std::io::stdout().lock();
    match kvik_cli::run(&args, store, kvik::env::Env::system(), &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e @ kvik_cli::Error::Usage(_)) => {
            eprintln!("kvik: {e}\n\n{}", kvik_cli::USAGE);
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("kvik: {e}");
            ExitCode::FAILURE
        }
    }
}
