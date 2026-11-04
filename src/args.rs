//! The 2.3.0 command line, parsed the way its `argparse` parser does: subcommands, long-option
//! abbreviations, `--opt=value`, `--`, negative numbers as values, and exit 2 on errors.
//! Usage and help texts are shorter than argparse's.

use crate::pyerr::{py_int, repr_str};

pub const COMMANDS: [&str; 7] = ["path", "resolve", "current", "find", "read", "list", "sources"];

#[derive(Default, Debug)]
pub struct Args {
    pub command: String,
    pub query: Option<String>,
    pub count: Option<i128>,
    pub context: Option<i128>,
    pub max_matches: Option<i128>,
    pub jobs: Option<i128>,
    pub line: Option<i128>,
    pub segment: Option<String>,
    pub record_sha256: Option<String>,
    pub legacy: bool,
    pub path_format: Option<String>,
    pub limit: Option<i128>,
    pub cursor: Option<String>,
    pub snapshot_dir: Option<String>,
    pub max_output_bytes: Option<i128>,
    pub json: bool,
    pub sources: Option<String>,
    pub source: Vec<String>,
}

pub enum Parsed {
    Run(Box<Args>),
    /// Print to stdout and exit 0 (`--version`, `--help`).
    Print(String),
    /// Print to stderr and exit 2.
    Error(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Flag,
    Value,
    Int,
    Append,
    Choice,
    Help,
}

fn options_for(command: &str) -> Vec<(&'static str, Kind)> {
    let mut o = vec![("-h", Kind::Help), ("--help", Kind::Help)];
    if matches!(command, "find" | "read") {
        o.push(("--context", Kind::Int));
    }
    if command == "find" {
        o.push(("--max-matches", Kind::Int));
        o.push(("--jobs", Kind::Int));
    }
    if command == "read" {
        o.push(("--line", Kind::Int));
        o.push(("--segment", Kind::Value));
        o.push(("--record-sha256", Kind::Value));
    }
    if matches!(command, "path" | "current") {
        o.push(("--legacy", Kind::Flag));
        o.push(("--path-format", Kind::Choice));
    }
    if matches!(command, "find" | "list") {
        o.push(("--limit", Kind::Int));
        o.push(("--cursor", Kind::Value));
        o.push(("--snapshot-dir", Kind::Value));
        o.push(("--max-output-bytes", Kind::Int));
    }
    o.push(("--json", Kind::Flag));
    o.push(("--sources", Kind::Value));
    o.push(("--source", Kind::Append));
    o
}

fn usage(command: &str) -> String {
    match command {
        "" => "usage: xsession [-h] [--version] {path,resolve,current,find,read,list,sources} ...".into(),
        c => {
            let mut parts = vec![format!("usage: xsession {} [-h]", c)];
            for (name, kind) in options_for(c).into_iter().skip(2) {
                let meta = name.trim_start_matches('-').replace('-', "_").to_uppercase();
                parts.push(match kind {
                    Kind::Flag => format!("[{}]", name),
                    Kind::Append => format!("[{} {}]", name, meta),
                    _ if name == "--line" => format!("{} {}", name, meta),
                    _ => format!("[{} {}]", name, meta),
                });
            }
            match c {
                "path" | "resolve" | "find" | "read" => parts.push("query".into()),
                "list" => parts.push("[count]".into()),
                _ => {}
            }
            parts.join(" ")
        }
    }
}

fn fail(command: &str, msg: &str) -> Parsed {
    let prog = if command.is_empty() { "xsession".to_string() } else { format!("xsession {}", command) };
    Parsed::Error(format!("{}\n{}: error: {}", usage(command), prog, msg))
}

/// argparse's `_negative_number_matcher`: `^-\d+$|^-\d*\.\d+$`.
fn negative_number(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('-') else { return false };
    if !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit()) {
        return true;
    }
    match rest.split_once('.') {
        Some((a, b)) => a.bytes().all(|c| c.is_ascii_digit()) && !b.is_empty() && b.bytes().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

enum Classified {
    Positional,
    Option(&'static str, Kind, Option<String>),
    Unknown,
    Ambiguous(String),
}

fn classify(arg: &str, options: &[(&'static str, Kind)]) -> Classified {
    if arg.is_empty() || !arg.starts_with('-') {
        return Classified::Positional;
    }
    if let Some(&(name, kind)) = options.iter().find(|(n, _)| *n == arg) {
        return Classified::Option(name, kind, None);
    }
    if arg.chars().count() == 1 {
        return Classified::Positional;
    }
    if let Some((head, value)) = arg.split_once('=') {
        if let Some(&(name, kind)) = options.iter().find(|(n, _)| *n == head) {
            return Classified::Option(name, kind, Some(value.to_string()));
        }
    }
    let (prefix, explicit) = match arg.split_once('=') {
        Some((h, v)) => (h, Some(v.to_string())),
        None => (arg, None),
    };
    let matches: Vec<&(&'static str, Kind)> = if arg.starts_with("--") {
        options.iter().filter(|(n, _)| n.starts_with(prefix)).collect()
    } else {
        // Single-dash: `-h` with an attached value, or an abbreviation of a single-dash option.
        options.iter().filter(|(n, _)| *n == &arg[..2.min(arg.len())] || (n.starts_with(prefix) && !n.starts_with("--"))).collect()
    };
    if matches.len() > 1 {
        let names: Vec<&str> = matches.iter().map(|(n, _)| *n).collect();
        return Classified::Ambiguous(format!("ambiguous option: {} could match {}", arg, names.join(", ")));
    }
    if let Some(&&(name, kind)) = matches.first() {
        return Classified::Option(name, kind, explicit);
    }
    if negative_number(arg) || arg.contains(' ') {
        return Classified::Positional;
    }
    Classified::Unknown
}

fn looks_optional(arg: &str, options: &[(&'static str, Kind)]) -> bool {
    !matches!(classify(arg, options), Classified::Positional) || arg == "--"
}

fn parse_int(option: &str, value: &str) -> Result<i128, String> {
    py_int(value).map_err(|_| format!("argument {}: invalid int value: {}", option, repr_str(value)))
}

fn help_text(command: &str) -> String {
    if command.is_empty() {
        format!("{}\n\nRead-only, on-demand transcript retrieval.\n\ncommands: path, resolve, current, find, read, list, sources\n", usage(""))
    } else {
        format!("{}\n", usage(command))
    }
}

pub fn parse(argv: &[String]) -> Parsed {
    // Top level: --version and --help may come before the subcommand.
    let top: [(&'static str, Kind); 3] = [("-h", Kind::Help), ("--help", Kind::Help), ("--version", Kind::Flag)];
    let mut i = 0;
    let mut extras: Vec<String> = Vec::new();
    let command = loop {
        let Some(arg) = argv.get(i) else {
            return fail("", "the following arguments are required: command");
        };
        i += 1;
        if arg == "--" {
            match argv.get(i) {
                Some(c) => {
                    i += 1;
                    break c.clone();
                }
                None => return fail("", "the following arguments are required: command"),
            }
        }
        match classify(arg, &top) {
            Classified::Positional => break arg.clone(),
            Classified::Option("--version", _, None) => return Parsed::Print(format!("{}\n", crate::inventory::VERSION)),
            Classified::Option("--version", _, Some(v)) => {
                return fail("", &format!("argument --version: ignored explicit argument {}", repr_str(&v)))
            }
            Classified::Option(_, _, _) => return Parsed::Print(help_text("")),
            Classified::Ambiguous(m) => return fail("", &m),
            Classified::Unknown => extras.push(arg.clone()),
        }
    };
    if !COMMANDS.contains(&command.as_str()) {
        let choices = COMMANDS.iter().map(|c| format!("'{}'", c)).collect::<Vec<_>>().join(", ");
        return fail("", &format!("argument command: invalid choice: {} (choose from {})", repr_str(&command), choices));
    }
    let options = options_for(&command);
    let mut a = Args { command: command.clone(), ..Default::default() };
    if matches!(command.as_str(), "find" | "read") {
        a.context = Some(0);
    }
    if command == "find" {
        a.max_matches = Some(10000);
    }
    if matches!(command.as_str(), "find" | "list") {
        a.limit = Some(20);
        a.max_output_bytes = Some(262144);
    }
    if command == "list" {
        a.count = Some(15);
    }
    let mut positionals: Vec<String> = Vec::new();
    let mut only_positional = false;
    while i < argv.len() {
        let arg = &argv[i];
        i += 1;
        if only_positional {
            positionals.push(arg.clone());
            continue;
        }
        if arg == "--" {
            only_positional = true;
            continue;
        }
        match classify(arg, &options) {
            Classified::Positional => positionals.push(arg.clone()),
            Classified::Ambiguous(m) => return fail(&command, &m),
            Classified::Unknown => extras.push(arg.clone()),
            Classified::Option(name, kind, explicit) => {
                if kind == Kind::Help {
                    return Parsed::Print(help_text(&command));
                }
                if kind == Kind::Flag {
                    if let Some(v) = explicit {
                        return fail(&command, &format!("argument {}: ignored explicit argument {}", name, repr_str(&v)));
                    }
                    match name {
                        "--json" => a.json = true,
                        _ => a.legacy = true,
                    }
                    continue;
                }
                let value = match explicit {
                    Some(v) => v,
                    None => match argv.get(i) {
                        Some(next) if !looks_optional(next, &options) => {
                            i += 1;
                            next.clone()
                        }
                        _ => return fail(&command, &format!("argument {}: expected one argument", name)),
                    },
                };
                match kind {
                    Kind::Int => {
                        let n = match parse_int(name, &value) {
                            Ok(n) => n,
                            Err(m) => return fail(&command, &m),
                        };
                        match name {
                            "--context" => a.context = Some(n),
                            "--max-matches" => a.max_matches = Some(n),
                            "--jobs" => a.jobs = Some(n),
                            "--line" => a.line = Some(n),
                            "--limit" => a.limit = Some(n),
                            _ => a.max_output_bytes = Some(n),
                        }
                    }
                    Kind::Choice => {
                        if !["native", "windows", "msys", "wsl"].contains(&value.as_str()) {
                            return fail(&command, &format!(
                                "argument --path-format: invalid choice: {} (choose from 'native', 'windows', 'msys', 'wsl')", repr_str(&value)));
                        }
                        a.path_format = Some(value);
                    }
                    Kind::Append => a.source.push(value),
                    _ => match name {
                        "--segment" => a.segment = Some(value),
                        "--record-sha256" => a.record_sha256 = Some(value),
                        "--cursor" => a.cursor = Some(value),
                        "--snapshot-dir" => a.snapshot_dir = Some(value),
                        _ => a.sources = Some(value),
                    },
                }
            }
        }
    }
    let mut positionals = positionals.into_iter();
    match command.as_str() {
        "path" | "resolve" | "find" | "read" => a.query = positionals.next(),
        "list" => {
            if let Some(c) = positionals.next() {
                match parse_int("count", &c) {
                    Ok(n) => a.count = Some(n),
                    Err(m) => return fail(&command, &m),
                }
            }
        }
        _ => {}
    }
    extras.extend(positionals);
    let mut missing = Vec::new();
    if matches!(command.as_str(), "path" | "resolve" | "find" | "read") && a.query.is_none() {
        missing.push("query");
    }
    if command == "read" && a.line.is_none() {
        missing.push("--line");
    }
    if !missing.is_empty() {
        return fail(&command, &format!("the following arguments are required: {}", missing.join(", ")));
    }
    if !extras.is_empty() {
        return fail("", &format!("unrecognized arguments: {}", extras.join(" ")));
    }
    Parsed::Run(Box::new(a))
}
