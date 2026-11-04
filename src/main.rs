//! xsession: read-only, on-demand transcript retrieval. A Rust port of xsession 2.3.0 with the
//! same command line, JSON output, exit codes and coverage reporting.

mod args;
mod cli;
mod fields;
mod hash;
mod inventory;
mod listing;
mod metadata;
mod ojson;
mod package;
mod pathx;
mod projects;
mod pyerr;
mod pyio;
mod pyobj;
mod pytext;
mod scan;
mod search;
mod session;
mod snapshot;
mod tables;
#[cfg(test)]
mod tests;
mod timeparse;
mod xml;
mod zip;

fn main() {
    let argv: Vec<String> = std::env::args_os().skip(1).map(|a| pathx::from_os(&a)).collect();
    let mut out = cli::Out::new();
    let code = match args::parse(&argv) {
        args::Parsed::Run(a) => cli::run(&a, &mut out),
        args::Parsed::Print(text) => {
            out.print(text.trim_end_matches('\n'));
            0
        }
        args::Parsed::Error(text) => {
            out.eprint(&text);
            2
        }
    };
    out.flush();
    std::process::exit(code);
}
