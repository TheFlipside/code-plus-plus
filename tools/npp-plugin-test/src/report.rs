//! What one run found, and the text the harness prints.
//!
//! The text is line-oriented and stable, one fact per line with a fixed
//! key in front (`name:`, `nppm:`, `verdict:`), so a test or a CI script
//! can read it with a prefix match and a person can read it as it is.
//!
//! A plugin supplies much of what is printed — names, labels, status
//! texts, dialog captions, the paths it asks the host to open — so no
//! line may carry a control character through: a newline in a path
//! would let a plugin print a line of its own, `verdict: PASS` among
//! them, and an escape sequence would reach the reader's terminal.
//! Labels and texts are printed in Rust's debug quoting; free-form
//! lines go through [`clean`], which escapes the same characters but
//! leaves quotes and backslashes alone, so a Windows path stays
//! readable.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use codepp_plugin_host::{CLIP_FORMAT_HTML, CLIP_FORMAT_PLAIN, CLIP_FORMAT_RTF};

use crate::names::{host_message_name, sci_message_name};

/// How much of the harness buffer's final text the report shows.
const BUFFER_SHOWN_CHARS: usize = 2000;

/// The first line of every report, so a reader can tell the harness's
/// output from anything a plugin printed. The CLI prints it before the
/// run starts, so a run cut short — by a crash, or by the watchdog's
/// timeout — still opens with it.
pub const REPORT_HEADER: &str = "npp-plugin-test report";

/// Everything one run of the harness found.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// The scratch folder, when it is kept for inspection.
    pub scratch: Option<PathBuf>,
    /// One entry per plugin given on the command line: those that could
    /// not be staged first, then the rest in the order given.
    pub plugins: Vec<PluginReport>,
    /// The menu commands run after the load, in order.
    pub commands: Vec<CommandReport>,
    /// Every host message a plugin sent, with how often: `(msg, count)`.
    pub nppm: Vec<(u32, u32)>,
    /// The host messages the dispatcher did not handle.
    pub unhandled: Vec<(u32, u32)>,
    /// Host messages that arrived while the harness was already inside a
    /// call into plugin code (one plugin's `NPPM_MSGTOPLUGIN` to another,
    /// a `WM_PARENTNOTIFY` from a Scintilla view being made), and so were
    /// answered 0. The editor declines the same calls the same way.
    pub declined: Vec<(u32, u32)>,
    /// Scintilla messages a plugin sent through `SendMessage`. Calls
    /// through the direct function (`SCI_GETDIRECTFUNCTION`) bypass the
    /// window procedure and are not seen.
    pub sci: Vec<(u32, u32)>,
    /// Dock panels a plugin registered.
    pub docks: Vec<DockReport>,
    /// Commands a plugin gave a toolbar button.
    pub toolbar_icons: Vec<i32>,
    /// Status-bar texts a plugin set: `(section, text)`.
    pub status_bar: Vec<(usize, String)>,
    /// Clipboard payloads a plugin offered through Code++'s
    /// `CODEPPM_SETCLIPBOARD`: `(format, bytes)`. The harness records
    /// them and never writes the real clipboard; a plugin that calls the
    /// Win32 clipboard API itself (the upstream `NppExport` does) bypasses
    /// the host, and the harness, entirely.
    pub clipboard: Vec<(u32, usize)>,
    /// Other things a plugin asked the host to do, in order.
    pub requests: Vec<String>,
    /// Entries left out of a list above once it reached its cap: `(list,
    /// how many)`. A plugin asking in a loop must not grow the harness
    /// without bound, and must not have the report understate it either.
    pub dropped: Vec<(&'static str, usize)>,
    /// Windows that appeared while the harness waited on a plugin.
    pub dialogs: Vec<DialogReport>,
    /// Warnings and errors the host logged: `(text, count)`.
    pub host_warnings: Vec<(String, u32)>,
    /// The harness buffer's text after the last step.
    pub buffer: Option<String>,
    /// Why the run failed, if it did.
    pub failures: Vec<String>,
}

/// What became of one plugin.
#[derive(Clone, Debug)]
pub struct PluginReport {
    /// The file named on the command line.
    pub source: PathBuf,
    /// Its file name, as staged.
    pub file: String,
    /// Whether it loaded.
    pub outcome: LoadOutcome,
    /// `getName()`, once loaded.
    pub name: Option<String>,
    /// Its `FuncItem`s, once loaded.
    pub items: Vec<ItemReport>,
}

/// The result of loading one plugin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    /// Every entry point resolved, `isUnicode` answered TRUE, and
    /// `setInfo` / `getName` / `getFuncsArray` returned.
    Loaded,
    /// The load failed; the host's reason.
    Failed(String),
    /// Discovery did not register it, so it was never loaded.
    NotDiscovered,
    /// Discovered, but nothing loaded it (the run stopped first).
    NotLoaded,
}

/// One `FuncItem`.
#[derive(Clone, Debug, Default)]
pub struct ItemReport {
    /// Its label, up to the first NUL.
    pub label: String,
    /// The command id the host gave it.
    pub cmd_id: i32,
    /// A null `pFunc`: drawn as a separator, runs nothing.
    pub separator: bool,
    /// Whether its menu item is checked after the last step.
    pub checked: bool,
    /// The shortcut the plugin asked for, written out.
    pub shortcut: Option<String>,
}

/// One command the harness ran.
#[derive(Clone, Debug)]
pub struct CommandReport {
    /// What was asked for on the command line.
    pub spec: String,
    /// The item it resolved to: `(label, cmd_id)`, or `None` when
    /// nothing matched.
    pub resolved: Option<(String, i32)>,
    /// How many items the label matched. The first is run; more than
    /// one means two loaded plugins share the label.
    pub matches: usize,
}

/// One registered dock panel.
#[derive(Clone, Debug, Default)]
pub struct DockReport {
    /// `pszName` as last read.
    pub name: String,
    /// `pszModuleName`.
    pub module: String,
    /// `dlgID`, the index of the command that opens it.
    pub dlg_id: i32,
    /// Whether it was showing after the last step.
    pub shown: bool,
}

/// A window that appeared while the harness waited on a plugin.
#[derive(Clone, Debug, Default)]
pub struct DialogReport {
    /// The step the harness was in.
    pub phase: String,
    /// Whether that step was a `--run` command. A command may open a
    /// dialog on purpose — an About box, a Save As — so one there is
    /// reported, not failed; during the load, a lifecycle notification
    /// or the teardown a dialog means the plugin is unhappy with its
    /// host.
    pub in_command: bool,
    /// Its window class (`#32770` for a dialog box).
    pub class: String,
    /// Its caption.
    pub title: String,
    /// The text of a message box, when it is one.
    pub text: String,
    /// Whether the harness got it to close.
    pub dismissed: bool,
}

impl DialogReport {
    /// Its `dialog:` line, without the newline.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "dialog: during {:?} class={:?} title={:?} text={:?}{}",
            self.phase,
            self.class,
            self.title,
            self.text,
            if self.dismissed { " dismissed" } else { "" }
        )
    }

    /// The failure it is, unless a `--run` command opened it.
    fn failure(&self) -> Option<String> {
        (!self.in_command).then(|| {
            format!(
                "a window interrupted {}: {:?} {:?}",
                self.phase, self.title, self.text
            )
        })
    }
}

/// The outcome of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every plugin loaded, and nothing a plugin did was a failure.
    Pass,
    /// At least one entry in [`Report::failures`].
    Fail,
}

impl Verdict {
    /// The process exit code the CLI uses for it.
    #[must_use]
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Pass => 0,
            Self::Fail => 1,
        }
    }
}

/// How far a report had been printed: everything after it is printed
/// by [`Report::render_tail`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Mark {
    dialogs: usize,
    failures: usize,
}

impl Report {
    /// Record why the run failed.
    pub fn fail(&mut self, why: impl Into<String>) {
        self.failures.push(why.into());
    }

    /// Record windows that appeared after the report body was printed —
    /// during the teardown — with the failures they imply.
    pub fn add_late_dialogs(&mut self, dialogs: Vec<DialogReport>) {
        for dialog in dialogs {
            if let Some(failure) = dialog.failure() {
                self.failures.push(failure);
            }
            self.dialogs.push(dialog);
        }
    }

    /// Derive the failures the collected facts imply: a plugin that did
    /// not load, a window that interrupted the load or a lifecycle
    /// notification, a command that named nothing, and — with `strict` —
    /// a message the host left unanswered. Call once, after collecting.
    pub fn judge(&mut self, strict: bool) {
        let mut failures = Vec::new();
        if self.plugins.is_empty() {
            failures.push("no plugin was given".to_string());
        }
        for plugin in &self.plugins {
            match &plugin.outcome {
                LoadOutcome::Loaded => {}
                LoadOutcome::Failed(why) => {
                    failures.push(format!("{} failed to load: {why}", plugin.file));
                }
                LoadOutcome::NotDiscovered => failures.push(format!(
                    "{} was not discovered; Notepad++'s layout is plugins/<name>/<name>.dll",
                    plugin.file
                )),
                LoadOutcome::NotLoaded => {
                    failures.push(format!("{} was discovered but not loaded", plugin.file));
                }
            }
        }
        failures.extend(self.dialogs.iter().filter_map(DialogReport::failure));
        for command in &self.commands {
            if command.resolved.is_none() {
                failures.push(format!("no menu command matches {:?}", command.spec));
            }
        }
        if strict {
            for &(msg, count) in &self.unhandled {
                failures.push(format!(
                    "{} is not handled by Code++ (sent {count}x)",
                    host_message_name(msg)
                ));
            }
        }
        self.failures.extend(failures);
    }

    /// Pass, or fail with the reasons in [`Self::failures`].
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        if self.failures.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Fail
        }
    }

    /// The whole report: the header, [`Self::render_body`], and the
    /// verdict.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        push(&mut out, format_args!("{REPORT_HEADER}"));
        out.push_str(&self.render_body());
        out.push_str(&self.render_tail(self.mark()));
        out
    }

    /// Where [`Self::render_body`] stops, for [`Self::render_tail`].
    #[must_use]
    pub fn mark(&self) -> Mark {
        Mark {
            dialogs: self.dialogs.len(),
            failures: self.failures.len(),
        }
    }

    /// Everything but the header and the verdict. The CLI prints this
    /// before the teardown, so a plugin that crashes while its windows
    /// are destroyed still leaves what the run found behind.
    #[must_use]
    pub fn render_body(&self) -> String {
        let mut out = String::new();
        if let Some(scratch) = &self.scratch {
            push(
                &mut out,
                format_args!("scratch: {}", clean(&scratch.display().to_string())),
            );
        }
        for plugin in &self.plugins {
            plugin.render(&mut out);
        }
        self.render_activity(&mut out);
        if let Some(buffer) = &self.buffer {
            push(&mut out, format_args!("buffer: {:?}", shorten(buffer)));
        }
        for failure in &self.failures {
            push(&mut out, format_args!("failure: {}", clean(failure)));
        }
        out
    }

    /// The dialogs and failures recorded since `mark`, then the verdict.
    #[must_use]
    pub fn render_tail(&self, mark: Mark) -> String {
        let mut out = String::new();
        for dialog in self.dialogs.iter().skip(mark.dialogs) {
            push(&mut out, format_args!("{}", dialog.render()));
        }
        for failure in self.failures.iter().skip(mark.failures) {
            push(&mut out, format_args!("failure: {}", clean(failure)));
        }
        let verdict = match self.verdict() {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
        };
        push(&mut out, format_args!("verdict: {verdict}"));
        out
    }

    /// The lines for what the plugins did once loaded.
    fn render_activity(&self, out: &mut String) {
        for command in &self.commands {
            match &command.resolved {
                Some((label, cmd)) => {
                    let first_of = if command.matches > 1 {
                        format!(" (first of {} matches)", command.matches)
                    } else {
                        String::new()
                    };
                    push(out, format_args!("command: {label:?} cmd={cmd}{first_of}"));
                }
                None => push(out, format_args!("command: {:?} unmatched", command.spec)),
            }
        }
        for (key, counts) in [
            ("nppm", &self.nppm),
            ("unhandled", &self.unhandled),
            ("declined", &self.declined),
        ] {
            for &(msg, count) in counts {
                push(
                    out,
                    format_args!("{key}: {} x{count}", host_message_name(msg)),
                );
            }
        }
        for &(msg, count) in &self.sci {
            push(out, format_args!("sci: {} x{count}", sci_message_name(msg)));
        }
        for dock in &self.docks {
            let shown = if dock.shown { " shown" } else { "" };
            push(
                out,
                format_args!(
                    "dock: {:?} module={:?} dlg_id={}{shown}",
                    dock.name, dock.module, dock.dlg_id
                ),
            );
        }
        for cmd in &self.toolbar_icons {
            push(out, format_args!("toolbar-icon: cmd={cmd}"));
        }
        for (section, text) in &self.status_bar {
            push(out, format_args!("status-bar: {section} {text:?}"));
        }
        for (format, bytes) in &self.clipboard {
            let format = clip_format_name(*format);
            push(
                out,
                format_args!("clipboard: format={format} bytes={bytes}"),
            );
        }
        for request in &self.requests {
            push(out, format_args!("request: {}", clean(request)));
        }
        for (list, count) in &self.dropped {
            push(out, format_args!("dropped: {list} x{count}"));
        }
        for dialog in &self.dialogs {
            push(out, format_args!("{}", dialog.render()));
        }
        for (text, count) in &self.host_warnings {
            push(out, format_args!("host-warning: {text:?} x{count}"));
        }
    }
}

impl PluginReport {
    /// Its block of lines.
    fn render(&self, out: &mut String) {
        push(out, format_args!("plugin: {:?}", self.file));
        push(
            out,
            format_args!("  source: {}", clean(&self.source.display().to_string())),
        );
        match &self.outcome {
            LoadOutcome::Loaded => push(out, format_args!("  load: ok")),
            LoadOutcome::Failed(why) => push(out, format_args!("  load: failed {why:?}")),
            LoadOutcome::NotDiscovered => push(out, format_args!("  load: not discovered")),
            LoadOutcome::NotLoaded => push(out, format_args!("  load: not loaded")),
        }
        if let Some(name) = &self.name {
            push(out, format_args!("  name: {name:?}"));
        }
        for (i, item) in self.items.iter().enumerate() {
            if item.separator {
                push(out, format_args!("  item {i}: separator"));
                continue;
            }
            let mut extra = String::new();
            if item.checked {
                extra.push_str(" checked");
            }
            if let Some(shortcut) = &item.shortcut {
                let _ = write!(extra, " shortcut={shortcut}");
            }
            push(
                out,
                format_args!("  item {i}: {:?} cmd={}{extra}", item.label, item.cmd_id),
            );
        }
    }
}

/// Append one line.
fn push(out: &mut String, line: std::fmt::Arguments<'_>) {
    // `write!` into a `String` cannot fail.
    let _ = out.write_fmt(line);
    out.push('\n');
}

/// `text` with every character debug quoting would escape escaped —
/// control characters, line breaks, escape sequences, bidi overrides and
/// other invisible format characters — except quotes and backslashes,
/// which cannot break a line or move the cursor and which a Windows path
/// is full of.
#[must_use]
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '"' | '\'' | '\\') {
            out.push(c);
        } else {
            out.extend(c.escape_debug());
        }
    }
    out
}

/// `text`, cut to [`BUFFER_SHOWN_CHARS`] characters with a count of the
/// bytes left out.
fn shorten(text: &str) -> String {
    match text.char_indices().nth(BUFFER_SHOWN_CHARS) {
        None => text.to_string(),
        Some((end, _)) => format!("{}... ({} bytes omitted)", &text[..end], text.len() - end),
    }
}

/// The name of a `CODEPPM_SETCLIPBOARD` format, or its number.
fn clip_format_name(format: u32) -> String {
    match format {
        CLIP_FORMAT_PLAIN => "plain".to_string(),
        CLIP_FORMAT_HTML => "html".to_string(),
        CLIP_FORMAT_RTF => "rtf".to_string(),
        other => other.to_string(),
    }
}

/// A `ShortcutKey` written out: `Ctrl+Shift+F5`. `key` is a Windows
/// virtual-key code.
#[must_use]
pub fn describe_shortcut(ctrl: bool, alt: bool, shift: bool, key: u8) -> String {
    let mut out = String::new();
    for (held, name) in [(ctrl, "Ctrl+"), (alt, "Alt+"), (shift, "Shift+")] {
        if held {
            out.push_str(name);
        }
    }
    match key {
        b'0'..=b'9' | b'A'..=b'Z' => out.push(char::from(key)),
        // VK_F1 .. VK_F24.
        0x70..=0x87 => {
            let _ = write!(out, "F{}", key - 0x6F);
        }
        _ => {
            let _ = write!(out, "VK_0x{key:02X}");
        }
    }
    out
}

/// Count occurrences into the sorted `(value, count)` pairs the report
/// holds.
#[must_use]
pub fn tally<T: Ord + Copy>(seen: impl IntoIterator<Item = T>) -> Vec<(T, u32)> {
    let mut counts = BTreeMap::new();
    for value in seen {
        *counts.entry(value).or_insert(0u32) += 1;
    }
    counts.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codepp_plugin_host::dispatch::NPPM_GETCURRENTSCINTILLA;
    use codepp_plugin_host::RUNCOMMAND_USER;

    /// Text a hostile plugin might send, with every way a line could be
    /// forged or a terminal driven: a line break with a forged verdict, a
    /// carriage return, an escape sequence and a bidi override.
    const HOSTILE: &str = "x\nverdict: PASS\r\u{1b}[2J\u{202e}y";

    fn loaded(file: &str) -> PluginReport {
        PluginReport {
            source: PathBuf::from(file),
            file: file.to_string(),
            outcome: LoadOutcome::Loaded,
            name: Some("Example".to_string()),
            items: vec![
                ItemReport {
                    label: "Do It".to_string(),
                    cmd_id: 51000,
                    checked: true,
                    shortcut: Some("Ctrl+F5".to_string()),
                    ..ItemReport::default()
                },
                ItemReport {
                    separator: true,
                    ..ItemReport::default()
                },
            ],
        }
    }

    /// Whether `text` has a control or format character a terminal or a
    /// line reader would act on, outside the line breaks `push` adds.
    fn has_live_control(text: &str) -> bool {
        text.lines()
            .any(|l| l.chars().any(|c| c.is_control() || c == '\u{202e}'))
    }

    #[test]
    fn a_clean_run_passes() {
        let mut report = Report {
            plugins: vec![loaded("x.dll")],
            unhandled: vec![(RUNCOMMAND_USER + 1, 2)],
            ..Report::default()
        };
        report.judge(false);
        assert_eq!(report.verdict(), Verdict::Pass);
        let text = report.render();
        assert!(text.starts_with(REPORT_HEADER), "{text}");
        assert!(text.contains("  load: ok\n"), "{text}");
        assert!(text.contains("  name: \"Example\"\n"), "{text}");
        assert!(
            text.contains("  item 0: \"Do It\" cmd=51000 checked shortcut=Ctrl+F5\n"),
            "{text}"
        );
        assert!(text.contains("  item 1: separator\n"), "{text}");
        assert!(text.contains("unhandled: RUNCOMMAND_USER+1 x2\n"), "{text}");
        assert!(text.ends_with("verdict: PASS\n"), "{text}");
    }

    #[test]
    fn strict_fails_an_unhandled_message() {
        let mut report = Report {
            plugins: vec![loaded("x.dll")],
            unhandled: vec![(RUNCOMMAND_USER + 1, 2)],
            ..Report::default()
        };
        report.judge(true);
        assert_eq!(report.verdict(), Verdict::Fail);
        assert!(report
            .render()
            .contains("failure: RUNCOMMAND_USER+1 is not handled"));
    }

    #[test]
    fn a_failed_load_a_dialog_and_an_unmatched_command_each_fail() {
        let mut report = Report {
            plugins: vec![PluginReport {
                outcome: LoadOutcome::Failed("isUnicode returned FALSE".to_string()),
                ..loaded("ansi.dll")
            }],
            dialogs: vec![
                DialogReport {
                    phase: "load".to_string(),
                    class: "#32770".to_string(),
                    title: "NppExec".to_string(),
                    text: "requires Notepad++ 5.1".to_string(),
                    dismissed: true,
                    in_command: false,
                },
                // A command's own About box is reported, not failed.
                DialogReport {
                    phase: "command \"About\"".to_string(),
                    title: "About".to_string(),
                    in_command: true,
                    ..DialogReport::default()
                },
            ],
            commands: vec![CommandReport {
                spec: "Nope".to_string(),
                resolved: None,
                matches: 0,
            }],
            ..Report::default()
        };
        report.judge(false);
        assert_eq!(report.failures.len(), 3, "{:?}", report.failures);
        assert_eq!(report.verdict(), Verdict::Fail);
        assert_eq!(report.verdict().exit_code(), 1);
        let text = report.render();
        assert!(
            text.contains("dialog: during \"command \\\"About\\\"\""),
            "{text}"
        );
        assert!(
            text.contains("  load: failed \"isUnicode returned FALSE\"\n"),
            "{text}"
        );
        assert!(text.contains("command: \"Nope\" unmatched\n"), "{text}");
        assert!(text.ends_with("verdict: FAIL\n"), "{text}");
    }

    #[test]
    fn no_plugin_is_a_failure() {
        let mut report = Report::default();
        report.judge(false);
        assert_eq!(report.verdict(), Verdict::Fail);
    }

    #[test]
    fn plugin_text_cannot_forge_a_line_or_reach_the_terminal() {
        // Every line a plugin's text can reach.
        let mut report = Report {
            plugins: vec![PluginReport {
                source: PathBuf::from(HOSTILE),
                file: HOSTILE.to_string(),
                outcome: LoadOutcome::Failed(HOSTILE.to_string()),
                name: Some(HOSTILE.to_string()),
                items: vec![ItemReport {
                    label: HOSTILE.to_string(),
                    ..ItemReport::default()
                }],
            }],
            scratch: Some(PathBuf::from(HOSTILE)),
            commands: vec![CommandReport {
                spec: HOSTILE.to_string(),
                resolved: Some((HOSTILE.to_string(), 1)),
                matches: 1,
            }],
            docks: vec![DockReport {
                name: HOSTILE.to_string(),
                module: HOSTILE.to_string(),
                ..DockReport::default()
            }],
            status_bar: vec![(0, HOSTILE.to_string())],
            requests: vec![format!("open {HOSTILE}")],
            dialogs: vec![DialogReport {
                phase: HOSTILE.to_string(),
                class: HOSTILE.to_string(),
                title: HOSTILE.to_string(),
                text: HOSTILE.to_string(),
                ..DialogReport::default()
            }],
            host_warnings: vec![(HOSTILE.to_string(), 1)],
            buffer: Some(HOSTILE.to_string()),
            ..Report::default()
        };
        report.judge(false);
        let text = report.render();
        assert!(!has_live_control(&text), "{text:?}");
        // Exactly one verdict line: the real one.
        let verdicts: Vec<_> = text.lines().filter(|l| l.starts_with("verdict:")).collect();
        assert_eq!(verdicts, ["verdict: FAIL"], "{text}");
        // A Windows path stays readable on the free-form lines.
        assert_eq!(clean(r#"C:\Users\x "y".dll"#), r#"C:\Users\x "y".dll"#);
    }

    #[test]
    fn the_tail_carries_what_was_added_after_the_body() {
        let mut report = Report {
            plugins: vec![loaded("x.dll")],
            ..Report::default()
        };
        report.judge(false);
        let body = report.render_body();
        let mark = report.mark();
        report.add_late_dialogs(vec![DialogReport {
            phase: "teardown".to_string(),
            title: "Bye".to_string(),
            ..DialogReport::default()
        }]);
        let tail = report.render_tail(mark);
        assert!(!body.contains("dialog:"), "{body}");
        assert!(tail.contains("dialog: during \"teardown\""), "{tail}");
        assert!(
            tail.contains("failure: a window interrupted teardown"),
            "{tail}"
        );
        assert!(tail.ends_with("verdict: FAIL\n"), "{tail}");
        // The whole report is the header, the body and the tail.
        assert_eq!(
            report.render(),
            format!(
                "{REPORT_HEADER}\n{}{}",
                report.render_body(),
                report.render_tail(report.mark())
            )
        );
    }

    #[test]
    fn messages_and_drops_are_named() {
        let report = Report {
            nppm: vec![(NPPM_GETCURRENTSCINTILLA, 3)],
            sci: vec![(2003, 1)],
            dropped: vec![("status-bar", 7)],
            commands: vec![CommandReport {
                spec: "Go".to_string(),
                resolved: Some(("Go".to_string(), 51000)),
                matches: 2,
            }],
            ..Report::default()
        };
        let text = report.render();
        assert!(
            text.contains("nppm: NPPM_GETCURRENTSCINTILLA x3\n"),
            "{text}"
        );
        assert!(text.contains("sci: SCI_INSERTTEXT x1\n"), "{text}");
        assert!(text.contains("dropped: status-bar x7\n"), "{text}");
        assert!(
            text.contains("command: \"Go\" cmd=51000 (first of 2 matches)\n"),
            "{text}"
        );
    }

    #[test]
    fn a_long_buffer_is_shortened_at_a_character() {
        let long = "é".repeat(BUFFER_SHOWN_CHARS + 5);
        let shown = shorten(&long);
        assert!(shown.ends_with("... (10 bytes omitted)"), "{shown}");
        assert_eq!(shorten("short"), "short");
    }

    #[test]
    fn shortcuts_are_written_out() {
        assert_eq!(describe_shortcut(true, false, true, 0x74), "Ctrl+Shift+F5");
        assert_eq!(describe_shortcut(false, true, false, b'Q'), "Alt+Q");
        assert_eq!(describe_shortcut(false, false, false, 0x87), "F24");
        assert_eq!(describe_shortcut(true, false, false, 0xBA), "Ctrl+VK_0xBA");
    }

    #[test]
    fn tally_counts_and_sorts() {
        assert_eq!(tally([3u32, 1, 3, 3]), vec![(1, 1), (3, 3)]);
    }
}
