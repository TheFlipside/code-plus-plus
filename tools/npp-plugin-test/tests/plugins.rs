//! The harness, run as the CLI it is, against real plugin DLLs.
//!
//! Each case starts `npp-plugin-test` as its own process, so a plugin
//! that crashes takes down that case alone and shows in its exit
//! status, and so the plugins' global state cannot leak between cases.
//!
//! The in-tree plugins (`example-hello` and the three clean-room
//! defaults) are found beside the harness binary, where `cargo build
//! --workspace` puts them. A case whose DLL is missing skips with a
//! message on a developer's machine and fails when `CI` is set, so CI
//! cannot go green on a build that never ran it. `cargo test -p
//! codepp-npp-plugin-test` does not rebuild those DLLs, so locally a
//! stale one from an earlier build is what gets tested; build the
//! workspace first. CI does (`.forgejo/workflows/ci.yml`).
//!
//! Third-party plugins — the real Notepad++ plugins DESIGN.md §5.6 asks
//! this harness to load, which cannot live in the tree (most are GPL) —
//! are named in `CODEPP_NPP_PLUGINS`, a `;`-separated list of DLL paths.
//! That is opt-in, under CI too: with it unset the case skips. Each
//! plugin must load, and its load and lifecycle must pass without a
//! dialog; their messages Code++ does not handle are reported, not
//! failed. See DEVELOPMENT.md.

#![cfg(target_os = "windows")]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use codepp_plugin_host::PLUGIN_CMD_ID_BASE;

const HARNESS: &str = env!("CARGO_BIN_EXE_npp-plugin-test");

/// The env var naming third-party plugin DLLs.
const THIRD_PARTY_VAR: &str = "CODEPP_NPP_PLUGINS";

/// A finished run: its exit status, its report and its stderr.
struct Outcome {
    code: Option<i32>,
    report: String,
    stderr: String,
}

impl Outcome {
    fn lines(&self, prefix: &str) -> Vec<&str> {
        self.report
            .lines()
            .filter(|l| l.starts_with(prefix))
            .collect()
    }

    fn has(&self, line: &str) -> bool {
        self.report.lines().any(|l| l == line)
    }

    /// The report and stderr, for an assertion's message.
    fn shown(&self) -> String {
        format!("{}\n--- stderr ---\n{}", self.report, self.stderr)
    }

    fn assert_pass(&self) {
        assert_eq!(self.code, Some(0), "{}", self.shown());
        assert!(self.has("verdict: PASS"), "{}", self.shown());
    }
}

/// Run the harness with `args`, plugins last.
fn run<S: AsRef<OsStr>>(args: &[S]) -> Outcome {
    let output = Command::new(HARNESS)
        .args(["--timeout", "20"])
        .args(args)
        .output()
        .expect("start the harness");
    Outcome {
        code: output.status.code(),
        report: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// An in-tree plugin's DLL beside the harness, or `None` (with a skip
/// message) when it was not built. Fails instead under CI.
fn in_tree(name: &str) -> Option<PathBuf> {
    let dll = Path::new(HARNESS)
        .parent()
        .expect("the harness has a folder")
        .join(format!("{name}.dll"));
    if dll.is_file() {
        return Some(dll);
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "{} is missing; CI must run `cargo build --workspace` before the tests",
        dll.display()
    );
    eprintln!(
        "skipping: {} is not built; run `cargo build --workspace` first",
        dll.display()
    );
    None
}

/// Run one in-tree plugin on `text`, selected, through `commands`.
fn run_on_text(dll: &Path, text: &str, commands: &[&str]) -> Outcome {
    let mut args: Vec<&OsStr> = vec![
        OsStr::new("--text"),
        OsStr::new(text),
        OsStr::new("--select-all"),
    ];
    for command in commands {
        args.push(OsStr::new("--run"));
        args.push(OsStr::new(command));
    }
    args.push(dll.as_os_str());
    run(&args)
}

#[test]
fn example_hello_loads_and_its_command_reaches_the_real_scintilla() {
    let Some(dll) = in_tree("example_hello") else {
        return;
    };
    let out = run(&[
        OsStr::new("--run"),
        OsStr::new("Insert Hello"),
        dll.as_os_str(),
    ]);
    out.assert_pass();
    assert!(out.has("  load: ok"), "{}", out.shown());
    assert!(out.has("  name: \"Example Hello\""), "{}", out.shown());
    assert!(
        out.has(&format!(
            "  item 0: \"Insert Hello\" cmd={PLUGIN_CMD_ID_BASE}"
        )),
        "{}",
        out.shown()
    );
    assert!(
        out.has(&format!(
            "command: \"Insert Hello\" cmd={PLUGIN_CMD_ID_BASE}"
        )),
        "{}",
        out.shown()
    );
    // The command asked the host which view is current, then sent the
    // text to that view through `SendMessage`, where the real Scintilla
    // inserted it at the caret.
    assert!(
        out.has("nppm: NPPM_GETCURRENTSCINTILLA x1"),
        "{}",
        out.shown()
    );
    assert!(out.has("sci: SCI_INSERTTEXT x1"), "{}", out.shown());
    let buffer = out.lines("buffer: ");
    assert!(
        buffer
            .first()
            .is_some_and(|b| b.starts_with("buffer: \"Hello from plugin")),
        "{}",
        out.shown()
    );
    // Its first panel is registered from NPPN_TBMODIFICATION, which only
    // reaches it if the load-time notifications were delivered.
    assert!(
        out.has("dock: \"Example Hello Panel\" module=\"example_hello.dll\" dlg_id=1"),
        "{}",
        out.shown()
    );
    // Nothing it sends goes unanswered, and nothing was declined.
    assert!(out.lines("unhandled: ").is_empty(), "{}", out.shown());
    assert!(out.lines("declined: ").is_empty(), "{}", out.shown());
}

#[test]
fn cppmimetools_encodes_base64_and_decodes_it_back() {
    let Some(dll) = in_tree("cppmimetools") else {
        return;
    };
    // Six bytes encode to eight characters with no padding, so the
    // expected text does not depend on whether the encoder pads.
    let encoded = run_on_text(&dll, "hello!", &["Base64 Encode"]);
    encoded.assert_pass();
    assert!(encoded.has("buffer: \"aGVsbG8h\""), "{}", encoded.shown());
    assert!(
        encoded.has("sci: SCI_REPLACETARGET x1"),
        "{}",
        encoded.shown()
    );
    assert!(encoded.has("  name: \"MIME Tools\""), "{}", encoded.shown());
    // Separators are null-`pFunc` items, reported as such.
    assert!(
        encoded
            .lines("  item ")
            .iter()
            .any(|l| l.ends_with(": separator")),
        "{}",
        encoded.shown()
    );

    let decoded = run_on_text(&dll, "aGVsbG8h", &["Base64 Decode"]);
    decoded.assert_pass();
    assert!(decoded.has("buffer: \"hello!\""), "{}", decoded.shown());

    // And the two in one run, each on the whole buffer.
    let both = run_on_text(&dll, "hello!", &["Base64 Encode", "Base64 Decode"]);
    both.assert_pass();
    assert!(both.has("buffer: \"hello!\""), "{}", both.shown());
    assert!(both.has("sci: SCI_REPLACETARGET x2"), "{}", both.shown());
}

#[test]
fn cppconverter_converts_to_hex_and_back() {
    let Some(dll) = in_tree("cppconverter") else {
        return;
    };
    let encoded = run_on_text(&dll, "hello", &["ASCII -> HEX"]);
    encoded.assert_pass();
    assert!(
        encoded.has("buffer: \"68 65 6C 6C 6F\""),
        "{}",
        encoded.shown()
    );
    assert!(
        encoded.has("sci: SCI_REPLACETARGET x1"),
        "{}",
        encoded.shown()
    );

    let decoded = run_on_text(&dll, "68 65 6C 6C 6F", &["HEX -> ASCII"]);
    decoded.assert_pass();
    assert!(decoded.has("buffer: \"hello\""), "{}", decoded.shown());
}

#[test]
fn cppexport_hands_its_html_to_the_host() {
    let Some(dll) = in_tree("cppexport") else {
        return;
    };
    let out = run(&[
        OsStr::new("--run"),
        OsStr::new("Copy HTML to Clipboard"),
        dll.as_os_str(),
    ]);
    out.assert_pass();
    // Through Code++'s own extension message, which the harness records
    // rather than putting on the real clipboard.
    assert!(out.has("nppm: CODEPPM_SETCLIPBOARD x1"), "{}", out.shown());
    let offered = out.lines("clipboard: format=html bytes=");
    assert_eq!(offered.len(), 1, "{}", out.shown());
    let bytes: usize = offered[0]
        .trim_start_matches("clipboard: format=html bytes=")
        .parse()
        .expect("a byte count");
    assert!(bytes > 0, "{}", out.shown());
    assert!(
        out.has("status-bar: 0 \"Export: HTML copied to clipboard\""),
        "{}",
        out.shown()
    );
}

#[test]
fn the_four_in_tree_plugins_load_in_one_pass() {
    let dlls: Vec<PathBuf> = ["example_hello", "cppmimetools", "cppconverter", "cppexport"]
        .into_iter()
        .filter_map(in_tree)
        .collect();
    if dlls.len() < 4 {
        return;
    }
    let out = run(&dlls);
    out.assert_pass();
    assert_eq!(out.lines("  load: ok").len(), 4, "{}", out.shown());
    // Every item, separators included, takes the next command id in
    // load order, so across the four the ids are one unbroken run from
    // the base with none repeated. Load order is discovery's (plugin
    // folders sorted by name, as in the editor), not the command line's:
    // cppconverter loads first and its first item takes the base.
    let items = out.lines("  item ").len();
    let mut ids: Vec<i32> = out
        .lines("  item ")
        .iter()
        .filter_map(|l| l.rsplit_once(" cmd=").map(|(_, id)| id))
        .map(|id| {
            id.split(' ')
                .next()
                .unwrap_or(id)
                .parse()
                .expect("a command id")
        })
        .collect();
    let commands = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        commands,
        "a command id repeats:
{}",
        out.shown()
    );
    assert_eq!(ids.first(), Some(&PLUGIN_CMD_ID_BASE), "{}", out.shown());
    let last = PLUGIN_CMD_ID_BASE + i32::try_from(items).expect("item count") - 1;
    assert_eq!(ids.last(), Some(&last), "{}", out.shown());
    assert!(
        out.has(&format!(
            "  item 0: \"ASCII -> HEX\" cmd={PLUGIN_CMD_ID_BASE}"
        )),
        "{}",
        out.shown()
    );
}

#[test]
fn a_file_that_is_not_a_plugin_fails_cleanly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bogus = dir.path().join("Bogus.dll");
    std::fs::write(&bogus, b"not a library").expect("write");
    let out = run(&[bogus.as_os_str()]);
    assert_eq!(out.code, Some(1), "{}", out.shown());
    assert!(out.has("verdict: FAIL"), "{}", out.shown());
    assert_eq!(out.lines("  load: failed ").len(), 1, "{}", out.shown());
}

#[test]
fn a_command_that_names_nothing_fails() {
    let Some(dll) = in_tree("example_hello") else {
        return;
    };
    let out = run(&[
        OsStr::new("--run"),
        OsStr::new("No Such Item"),
        dll.as_os_str(),
    ]);
    assert_eq!(out.code, Some(1), "{}", out.shown());
    assert!(
        out.has("command: \"No Such Item\" unmatched"),
        "{}",
        out.shown()
    );
}

#[test]
fn help_and_bad_arguments() {
    let help = Command::new(HARNESS)
        .arg("--help")
        .output()
        .expect("start the harness");
    assert_eq!(help.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&help.stdout),
        codepp_npp_plugin_test::options::USAGE
    );
    for args in [&[][..], &["x.dll", "--frobnicate"][..]] {
        let status = Command::new(HARNESS)
            .args(args)
            .output()
            .expect("start the harness")
            .status;
        assert_eq!(status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn third_party_plugins_load() {
    let Some(list) = std::env::var_os(THIRD_PARTY_VAR) else {
        eprintln!(
            "skipping: set {THIRD_PARTY_VAR} to a ;-separated list of Notepad++ plugin DLLs \
             to check them (see DEVELOPMENT.md)"
        );
        return;
    };
    let dlls: Vec<PathBuf> = std::env::split_paths(&list).collect();
    assert!(!dlls.is_empty(), "{THIRD_PARTY_VAR} names no plugin");
    // One process per plugin, every plugin run, the failures reported
    // together: a crash in one is that plugin's, and the next still runs.
    let mut failed = Vec::new();
    for dll in dlls {
        let out = run(&[OsStr::new("--"), dll.as_os_str()]);
        println!("{}", out.report);
        if out.code != Some(0) {
            failed.push(format!(
                "{} (exit {:?}):\n{}",
                dll.display(),
                out.code,
                out.shown()
            ));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
}
