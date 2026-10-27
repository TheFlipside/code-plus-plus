//! The command line.

use std::path::PathBuf;
use std::time::Duration;

/// The text the harness buffer starts with when `--text` is not given.
pub const DEFAULT_TEXT: &str = "Code++ plugin test buffer\r\nline two\r\n";

/// How long one step may take before the harness gives up, by default.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest `--timeout` taken, in seconds: a day. Nothing a harness
/// run waits for should take longer, and a bound keeps the arithmetic on
/// deadlines far from overflow.
pub const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// The usage text `--help` prints.
pub const USAGE: &str = "\
npp-plugin-test: host a Notepad++ plugin DLL against Code++'s plugin host

USAGE:
    npp-plugin-test [OPTIONS] <PLUGIN.dll>...

Each plugin is staged into a scratch folder in Notepad++'s layout
(plugins/<name>/<name>.dll; a DLL already in a folder named after it
brings that whole folder along) and loaded the way Code++ loads it:
isUnicode, getName, setInfo, getFuncsArray, then NPPN_TBMODIFICATION,
NPPN_BUFFERACTIVATED and NPPN_READY, then any --run commands, then
NPPN_BEFORESHUTDOWN and NPPN_SHUTDOWN, then the host window is destroyed.
The host window, its menus and both Scintilla views are real; every host
message a plugin sends goes through Code++'s own dispatcher and is
recorded, answered or not. What a plugin asks the host to
do outside the scratch folder (open a file, save elsewhere, set the
clipboard) is recorded, not done. A plugin's own code is another matter:
a --run command may call Windows directly — the upstream NppExport writes
the clipboard itself — so run third-party commands with that in mind.
A dialog that interrupts the load, a notification or the teardown is
closed and fails the run; one a --run command opens is closed and only
reported. Closing escalates from Close to Cancel, No and finally OK, so
an unattended run may accept a prompt.

OPTIONS:
    --run <ITEM>       After loading, run a plugin menu command: its label
                       (\"Base64 Encode\", with or without its & mnemonic) or
                       #N for the first plugin's Nth item. Repeatable; run in
                       order.
    --text <TEXT>      The buffer's initial text (default: two short lines).
    --select-all       Select the whole buffer before each --run command.
    --strict           Fail when a plugin sends a message Code++ does not
                       handle (by default these are only reported).
    --timeout <SECS>   Give up when one step takes longer (default 30, at
                       most 86400).
    --interactive      Show the host window and keep it open until it is
                       closed; the plugins' commands are on its Plugins menu.
    --keep             Keep the scratch folder and print its path; a kept
                       folder is never swept by a later run.
    --verbose          Log the host's tracing output to stderr; CODEPP_LOG
                       sets the filter (default info).
    -h, --help         Print this text.

EXIT STATUS:
    0 every plugin loaded and nothing failed; 1 a failure (see the
    `failure:` lines); 2 bad arguments; 3 a step timed out. A plugin that
    crashes the process ends it with the crash's own status.
";

/// What the command line asked for.
///
/// Each `bool` is one independent on/off flag of the command line, so
/// clippy's suggestion to fold them into a state machine does not apply.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The plugin DLLs, in the order given.
    pub plugins: Vec<PathBuf>,
    /// `--run`, in order.
    pub run: Vec<String>,
    /// `--text`.
    pub text: String,
    /// `--select-all`.
    pub select_all: bool,
    /// `--strict`.
    pub strict: bool,
    /// `--timeout`.
    pub timeout: Duration,
    /// `--interactive`.
    pub interactive: bool,
    /// `--keep`.
    pub keep: bool,
    /// `--verbose`.
    pub verbose: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            plugins: Vec::new(),
            run: Vec::new(),
            text: DEFAULT_TEXT.to_string(),
            select_all: false,
            strict: false,
            timeout: DEFAULT_TIMEOUT,
            interactive: false,
            keep: false,
            verbose: false,
        }
    }
}

/// The result of reading the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    /// Run with these options.
    Run(Options),
    /// `--help`.
    Help,
}

/// Read the arguments after the program name.
///
/// # Errors
///
/// An unknown option, an option missing its value, a bad `--timeout`,
/// or no plugin at all. The message says which.
pub fn parse(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Parsed, String> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    let mut only_paths = false;
    while let Some(arg) = args.next() {
        if only_paths {
            options.plugins.push(PathBuf::from(arg));
            continue;
        }
        let Some(flag) = arg.to_str().filter(|a| a.starts_with('-')) else {
            options.plugins.push(PathBuf::from(arg));
            continue;
        };
        let mut value = |name: &str| -> Result<String, String> {
            args.next()
                .ok_or_else(|| format!("{name} needs a value"))?
                .into_string()
                .map_err(|_| format!("{name}'s value is not valid Unicode"))
        };
        match flag {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--" => only_paths = true,
            "--run" => options.run.push(value("--run")?),
            "--text" => options.text = value("--text")?,
            "--select-all" => options.select_all = true,
            "--strict" => options.strict = true,
            "--interactive" => options.interactive = true,
            "--keep" => options.keep = true,
            "--verbose" => options.verbose = true,
            "--timeout" => {
                let secs = value("--timeout")?;
                let secs: u64 = secs
                    .parse()
                    .ok()
                    .filter(|&s| (1..=MAX_TIMEOUT_SECS).contains(&s))
                    .ok_or_else(|| {
                        format!(
                            "--timeout wants a whole number of seconds from 1 to \
                             {MAX_TIMEOUT_SECS}, not {secs:?}"
                        )
                    })?;
                options.timeout = Duration::from_secs(secs);
            }
            other => return Err(format!("unknown option {other:?}; see --help")),
        }
    }
    if options.plugins.is_empty() {
        return Err("no plugin given; see --help".to_string());
    }
    Ok(Parsed::Run(options))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Parsed, String> {
        parse(args.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn plugins_and_flags_are_read_in_any_order() {
        let Ok(Parsed::Run(options)) = parse_strs(&[
            "a.dll",
            "--run",
            "Base64 Encode",
            "--select-all",
            "b.dll",
            "--run",
            "#0",
            "--timeout",
            "5",
            "--strict",
        ]) else {
            panic!("should parse");
        };
        assert_eq!(
            options.plugins,
            [PathBuf::from("a.dll"), PathBuf::from("b.dll")]
        );
        assert_eq!(options.run, ["Base64 Encode", "#0"]);
        assert!(options.select_all && options.strict);
        assert_eq!(options.timeout, Duration::from_secs(5));
        assert_eq!(options.text, DEFAULT_TEXT);
    }

    #[test]
    fn a_double_dash_ends_the_options() {
        let Ok(Parsed::Run(options)) = parse_strs(&["--", "--odd-name.dll"]) else {
            panic!("should parse");
        };
        assert_eq!(options.plugins, [PathBuf::from("--odd-name.dll")]);
    }

    #[test]
    fn help_wins() {
        assert_eq!(parse_strs(&["x.dll", "--help"]), Ok(Parsed::Help));
        assert_eq!(parse_strs(&["-h"]), Ok(Parsed::Help));
    }

    #[test]
    fn mistakes_are_refused() {
        assert!(parse_strs(&[]).unwrap_err().contains("no plugin"));
        assert!(parse_strs(&["x.dll", "--run"])
            .unwrap_err()
            .contains("needs a value"));
        assert!(parse_strs(&["x.dll", "--timeout", "0"]).is_err());
        assert!(parse_strs(&["x.dll", "--timeout", "soon"]).is_err());
        assert!(parse_strs(&["x.dll", "--timeout", "86401"]).is_err());
        assert!(parse_strs(&["x.dll", "--timeout", "86400"]).is_ok());
        assert!(parse_strs(&["x.dll", "--frobnicate"])
            .unwrap_err()
            .contains("unknown option"));
    }
}
