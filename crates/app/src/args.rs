//! Command-line parsing.
//!
//! Hand-rolled rather than `clap`, for the same reason `ui_gtk` skips
//! `gtk::Application`: the surface is three flags and one path, and
//! DESIGN.md §8's 80 ms cold-start budget does not have room for an
//! argument-parsing framework's initialisation on the critical path to
//! the first frame.
//!
//! # Why this exists at all
//!
//! Before it, `main` did `std::env::args_os().nth(1)` and treated
//! whatever it found as a file path. That is fine until the moment a
//! flag is added — `codepp --perf` would have tried to open a file
//! literally named `--perf`, silently, with the failure surfacing as a
//! confusing "Open failed" dialog rather than a usage error.

use std::ffi::OsString;
use std::path::PathBuf;

/// Everything the binary accepts.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// A file to open at startup. Same effect as dropping it on the
    /// window.
    pub path: Option<PathBuf>,
    /// `--verbose`: turn the log sink on at `info`.
    pub verbose: bool,
    /// `--perf`: emit the DESIGN.md §8 measurements (cold start to
    /// first draw, keystroke latency). Implies [`Self::verbose`],
    /// since the measurements are delivered through the same sink.
    pub perf: bool,
}

/// What `main` should do after parsing.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// Run normally.
    Run(Box<Args>),
    /// Print `text` and exit successfully (`--help`).
    Exit(String),
    /// Print `text` to stderr and exit non-zero.
    Fail(String),
}

/// Usage text. Kept next to the parser so a flag cannot be added
/// without the help output noticing.
const USAGE: &str = "\
Code++ — a cross-platform Notepad++.

USAGE:
    codepp [OPTIONS] [FILE]

ARGS:
    <FILE>    File to open at startup.

OPTIONS:
    -v, --verbose    Log diagnostics to stderr at `info` level.
        --perf       Log the startup and keystroke-latency measurements
                     from DESIGN.md §8. Implies --verbose.
    -h, --help       Print this help and exit.
        --           Treat every later argument as a path, so a file
                     whose name begins with `-` can still be opened.

ENVIRONMENT:
    CODEPP_LOG    Log filter, e.g. `info`, `debug`,
                  `codepp_shell=debug,codepp_ui_gtk=trace`. Turns the
                  sink on by itself; --verbose is shorthand for
                  CODEPP_LOG=info.

                  It OVERRIDES --verbose rather than adding to it, so a
                  filter that names only other targets will silence
                  --perf. To keep both, include the perf target:
                  CODEPP_LOG=codepp_shell=debug,codepp::perf=info
";

/// Parse an argument list that does **not** include `argv[0]`.
///
/// Split from the process's real arguments so it can be unit-tested,
/// which matters more than it looks: the failure mode this replaces
/// (a flag silently parsed as a filename) is invisible at the type
/// level and only shows up as odd runtime behaviour.
pub fn parse<I: IntoIterator<Item = OsString>>(argv: I) -> Parsed {
    let mut out = Args::default();
    let mut positional: Vec<PathBuf> = Vec::new();
    let mut flags_ended = false;

    for raw in argv {
        // Only compare as text when the argument *is* text. A
        // non-UTF-8 path must still reach `positional` untouched
        // rather than being lossily converted and then opened by the
        // wrong name — which is exactly the bug class the display
        // sanitizer exists to keep out of chrome.
        let as_str = raw.to_str().map(str::to_owned);
        match as_str.as_deref() {
            Some(_) if flags_ended => positional.push(PathBuf::from(raw)),
            Some("--") => flags_ended = true,
            Some("-h" | "--help") => return Parsed::Exit(USAGE.to_string()),
            Some("-v" | "--verbose") => out.verbose = true,
            Some("--perf") => {
                out.perf = true;
                out.verbose = true;
            }
            // Reject unknown flags rather than opening them as files.
            // `-` alone is conventionally stdin, which Code++ has no
            // notion of, so it is an error too rather than a filename.
            Some(other) if other.starts_with('-') => {
                // `{other:?}` for the same reason `logging.rs` uses it:
                // this string is echoed to the terminal, and argv can
                // carry escape sequences when the process is launched
                // by a URI handler or an "Open With" association that
                // builds its arguments from external data.
                return Parsed::Fail(format!(
                    "unrecognised option {other:?}\n\nTry `codepp --help`."
                ));
            }
            _ => positional.push(PathBuf::from(raw)),
        }
    }

    if positional.len() > 1 {
        // Multi-file open is a real feature (Win32's
        // `OFN_ALLOWMULTISELECT` path already does it) but the
        // backends' `run` entry points take one optional path, so
        // accepting several here would silently drop all but the
        // first. Say so instead.
        return Parsed::Fail(format!(
            "expected at most one file, got {}\n\nTry `codepp --help`.",
            positional.len()
        ));
    }
    out.path = positional.pop();
    Parsed::Run(Box::new(out))
}

#[cfg(test)]
mod tests {
    use super::{parse, Args, Parsed};
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn run(argv: &[&str]) -> Parsed {
        parse(argv.iter().map(OsString::from))
    }

    fn args(argv: &[&str]) -> Args {
        match run(argv) {
            Parsed::Run(a) => *a,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn no_arguments_runs_with_nothing_set() {
        assert_eq!(args(&[]), Args::default());
    }

    #[test]
    fn a_bare_path_is_the_file_to_open() {
        assert_eq!(args(&["notes.txt"]).path, Some(PathBuf::from("notes.txt")));
    }

    #[test]
    fn flags_are_recognised_and_perf_implies_verbose() {
        assert!(args(&["--verbose"]).verbose);
        assert!(args(&["-v"]).verbose);
        let a = args(&["--perf"]);
        assert!(a.perf);
        assert!(
            a.verbose,
            "--perf must turn the sink on; its output goes through it"
        );
    }

    #[test]
    fn flags_and_a_path_combine_in_either_order() {
        let a = args(&["--perf", "notes.txt"]);
        assert!(a.perf);
        assert_eq!(a.path, Some(PathBuf::from("notes.txt")));
        let b = args(&["notes.txt", "--perf"]);
        assert!(b.perf);
        assert_eq!(b.path, Some(PathBuf::from("notes.txt")));
    }

    #[test]
    fn an_unknown_flag_is_an_error_not_a_filename() {
        // The whole point of this module. Before it, `--pref` (a typo
        // for `--perf`) would have been opened as a file.
        match run(&["--pref"]) {
            Parsed::Fail(msg) => assert!(msg.contains("--pref"), "{msg}"),
            other => panic!("expected Fail, got {other:?}"),
        }
        // `-` is not stdin here, and must not become a filename either.
        assert!(matches!(run(&["-"]), Parsed::Fail(_)));
    }

    #[test]
    fn double_dash_lets_a_dash_prefixed_file_be_opened() {
        let a = args(&["--", "--perf"]);
        assert_eq!(
            a.path,
            Some(PathBuf::from("--perf")),
            "after `--`, a flag-looking argument is a path"
        );
        assert!(!a.perf, "the flag after `--` must not also be parsed");
    }

    #[test]
    fn help_exits_successfully_with_usage() {
        match run(&["--help"]) {
            Parsed::Exit(text) => {
                assert!(text.contains("--perf"), "help must list every flag");
                assert!(text.contains("CODEPP_LOG"));
            }
            other => panic!("expected Exit, got {other:?}"),
        }
    }

    #[test]
    fn more_than_one_path_is_refused_rather_than_silently_dropped() {
        // `run` takes one optional path, so a second would vanish.
        match run(&["a.txt", "b.txt"]) {
            Parsed::Fail(msg) => assert!(msg.contains('2'), "{msg}"),
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_non_utf8_path_survives_parsing_byte_for_byte() {
        // Routine on Linux and the reason this parser matches on
        // `to_str()` rather than converting up front: a lossy
        // conversion here would try to open a *different* filename
        // than the user typed.
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let raw = OsString::from_vec(b"caf\xe9.txt".to_vec());
        let a = match parse(std::iter::once(raw.clone())) {
            Parsed::Run(a) => *a,
            other => panic!("expected Run, got {other:?}"),
        };
        assert_eq!(
            a.path.as_deref().map(|p| p.as_os_str().as_bytes()),
            Some(&b"caf\xe9.txt"[..])
        );
    }
}
