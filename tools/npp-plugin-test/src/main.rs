//! `npp-plugin-test` — see the library docs, or `--help`.

use std::io::Write as _;

use codepp_npp_plugin_test::options::{parse, Parsed, USAGE};

/// The exit status of a command line that could not be read.
const USAGE_EXIT_CODE: i32 = 2;

/// The exit status when the report could not be written: a run nobody
/// can read the result of has not passed.
#[cfg(target_os = "windows")]
const UNWRITTEN_EXIT_CODE: i32 = 1;

fn main() {
    let options = match parse(std::env::args_os().skip(1)) {
        Ok(Parsed::Run(options)) => options,
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return;
        }
        Err(e) => {
            eprintln!("npp-plugin-test: {e}");
            std::process::exit(USAGE_EXIT_CODE);
        }
    };
    std::process::exit(host(&options));
}

/// Print `text` and flush it, or say why not.
#[cfg(target_os = "windows")]
fn emit(text: &str) -> bool {
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .is_ok()
}

#[cfg(target_os = "windows")]
fn host(options: &codepp_npp_plugin_test::options::Options) -> i32 {
    use codepp_npp_plugin_test::report::REPORT_HEADER;

    // The header first, so a run cut short — a crash, or the watchdog's
    // timeout, which adds its own lines — still says what it is.
    let mut written = emit(&format!("{REPORT_HEADER}\n"));
    let mut run = codepp_npp_plugin_test::run(options);
    // The body before the teardown: a plugin that crashes while its
    // windows are destroyed still leaves what the run found behind, and
    // the crash shows in the exit status. The verdict comes last, once
    // the teardown has had its say.
    written &= emit(&run.report.render_body());
    let mark = run.report.mark();
    run.finish();
    written &= emit(&run.report.render_tail(mark));
    if written {
        run.report.verdict().exit_code()
    } else {
        UNWRITTEN_EXIT_CODE
    }
}

#[cfg(not(target_os = "windows"))]
fn host(_options: &codepp_npp_plugin_test::options::Options) -> i32 {
    let _ = writeln!(
        std::io::stderr(),
        "npp-plugin-test: hosting a Notepad++ plugin DLL needs Windows; \
         on this platform only the unit tests run"
    );
    USAGE_EXIT_CODE
}
