//! Fetching the update feed, for the signature of a download
//! (DESIGN.md §11.4).
//!
//! `WinSparkle` reads the feed, verifies the download against the
//! signature it found there, and then hands Code++ a path and nothing
//! else. Code++ verifies the file it is about to run itself, so it needs
//! that signature too, and this is how it gets it: one `GET` of the
//! feed's URL, made on a worker thread and only when an installer is
//! waiting to be run. A feed that changed since the library read it
//! carries another signature, the verification fails, and the update is
//! refused.
//!
//! **What is asked, and of whom.** The URL is compiled into the build
//! (DESIGN.md §11.7) and never read from a file or a setting.
//! [`fetch`] takes `https:` and nothing else, with one exception that
//! exists for tests: `http:` to `127.0.0.1`. A redirect is not followed:
//! the feed's address is fixed for good, and a server that answers with
//! one is not the feed. A certificate Windows does not trust ends the
//! request, which is `WinHTTP`'s default and is left alone; TLS older
//! than 1.2 is not offered, and the account's credentials are never
//! sent to a server or a proxy that asks for them. What comes back is
//! capped by the caller in size and here in time, read as bytes and
//! interpreted by nobody here.
//!
//! **Loaded lazily.** `winhttp.dll` is not imported: it is loaded from
//! System32 the first time a feed is fetched, as the plugin-panel key's
//! libraries are, so a start that installs nothing never maps it
//! (DESIGN.md §11.2).

use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{s, w, HSTRING, PCWSTR};
use windows::Win32::Foundation::GetLastError;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

/// How long each step of the request may take (resolving the name,
/// connecting, sending, and each wait for data), in milliseconds.
const STEP_TIMEOUT_MS: i32 = 15_000;

/// How long the whole body may take to arrive. The step timeout bounds
/// each wait, so a server that sends a byte now and then would
/// otherwise keep the updater's one worker for hours.
const BODY_TIMEOUT: Duration = Duration::from_mins(1);

/// The longest URL [`fetch`] takes, in bytes. The feed's own is under a
/// hundred.
const MAX_URL_BYTES: usize = 2048;

/// How much of the body is asked for at a time, in bytes.
const CHUNK_BYTES: u32 = 16 * 1024;

/// The size of a `DWORD`, as the calls that take one by pointer want it.
const DWORD_BYTES: u32 = 4;

/// Why a feed was not fetched.
#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The URL is not one [`fetch`] asks: what is wrong with it.
    Url(&'static str),
    /// `winhttp.dll`, or one of its functions, could not be found.
    Unavailable,
    /// A step of the request failed: the step, and Windows' error code.
    /// A server that cannot be reached, a name that does not resolve
    /// and a certificate that is not trusted all end here.
    Request(&'static str, u32),
    /// The server answered with a status other than 200, a redirect
    /// included.
    Status(u32),
    /// The body is longer than the caller's cap.
    TooLarge,
    /// The body took longer to arrive than it may.
    TooSlow,
    /// The body ended before the length the server announced for it.
    CutShort,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Url(why) => write!(f, "the feed's address cannot be used: {why}"),
            Self::Unavailable => f.write_str("WinHTTP is not available"),
            Self::Request(step, code) => write!(f, "the request failed ({step}, error {code})"),
            Self::Status(status) => write!(f, "the server answered with status {status}"),
            Self::TooLarge => f.write_str("the feed is larger than it may be"),
            Self::TooSlow => f.write_str("the feed took too long to arrive"),
            Self::CutShort => f.write_str("the feed ended before it was whole"),
        }
    }
}

impl std::error::Error for FetchError {}

/// A URL taken apart: what [`fetch`] connects to and asks for.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    secure: bool,
    host: String,
    port: u16,
    /// The path and query, starting with `/`.
    object: String,
}

/// Take `url` apart, refusing anything but a plain `https://host/path`
/// (or `http://127.0.0.1…`, for tests).
///
/// Strict on purpose. The URL is a constant of the build, so nothing is
/// lost by refusing forms it never has: user information, a fragment, a
/// host that is not letters, digits, dots and hyphens, a port that is
/// not a number, and any character outside printable ASCII.
fn parse_url(url: &str) -> Result<Target, FetchError> {
    if url.len() > MAX_URL_BYTES {
        return Err(FetchError::Url("it is too long"));
    }
    let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(FetchError::Url("it is not an https address"));
    };
    let (authority, object) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest, "/"),
    };
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => {
            let digits =
                !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit());
            let port = digits.then(|| port.parse::<u16>().ok()).flatten();
            match port {
                Some(port) if port != 0 => (host, port),
                _ => return Err(FetchError::Url("its port is not a port number")),
            }
        }
        None => (authority, if secure { 443 } else { 80 }),
    };
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
        && !host.contains("..");
    if !host_ok {
        return Err(FetchError::Url("its host is not a plain host name"));
    }
    if !secure && host != "127.0.0.1" {
        return Err(FetchError::Url("it is not an https address"));
    }
    // A `\` is refused with the rest: some clients and servers read it
    // as a `/`.
    if !object.bytes().all(|b| b.is_ascii_graphic()) || object.contains(['#', '\\']) {
        return Err(FetchError::Url("its path holds a character it may not"));
    }
    Ok(Target {
        secure,
        host: host.to_owned(),
        port,
        object: object.to_owned(),
    })
}

/// Whether [`fetch`] would ask `url` at all: a plain `https:` address,
/// or `http:` to `127.0.0.1`, as the module's documentation describes. A build whose feed is
/// anything else has no updater, since Code++ could never get a
/// download's signature from it.
#[must_use]
pub fn is_fetchable(url: &str) -> bool {
    parse_url(url).is_ok()
}

type Hinternet = *mut c_void;
type OpenFn = unsafe extern "system" fn(PCWSTR, u32, PCWSTR, PCWSTR, u32) -> Hinternet;
type SetTimeoutsFn = unsafe extern "system" fn(Hinternet, i32, i32, i32, i32) -> i32;
type ConnectFn = unsafe extern "system" fn(Hinternet, PCWSTR, u16, u32) -> Hinternet;
type OpenRequestFn = unsafe extern "system" fn(
    Hinternet,
    PCWSTR,
    PCWSTR,
    PCWSTR,
    PCWSTR,
    *const PCWSTR,
    u32,
) -> Hinternet;
type SetOptionFn = unsafe extern "system" fn(Hinternet, u32, *const c_void, u32) -> i32;
type SendRequestFn =
    unsafe extern "system" fn(Hinternet, PCWSTR, u32, *const c_void, u32, u32, usize) -> i32;
type ReceiveResponseFn = unsafe extern "system" fn(Hinternet, *mut c_void) -> i32;
type QueryHeadersFn =
    unsafe extern "system" fn(Hinternet, u32, PCWSTR, *mut c_void, *mut u32, *mut u32) -> i32;
type QueryDataAvailableFn = unsafe extern "system" fn(Hinternet, *mut u32) -> i32;
type ReadDataFn = unsafe extern "system" fn(Hinternet, *mut c_void, u32, *mut u32) -> i32;
type CloseHandleFn = unsafe extern "system" fn(Hinternet) -> i32;

/// `WINHTTP_ACCESS_TYPE_NO_PROXY`: connect directly.
const ACCESS_TYPE_NO_PROXY: u32 = 1;
/// `WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY`: the proxy the system is set
/// up with, if any (Windows 8.1 and later).
const ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
/// `WINHTTP_FLAG_SECURE`: the request is made over TLS.
const FLAG_SECURE: u32 = 0x0080_0000;
/// `WINHTTP_OPTION_REDIRECT_POLICY` and its value
/// `WINHTTP_OPTION_REDIRECT_POLICY_NEVER`.
const OPTION_REDIRECT_POLICY: u32 = 88;
const REDIRECT_POLICY_NEVER: u32 = 0;
/// `WINHTTP_OPTION_SECURE_PROTOCOLS`, and the flags for TLS 1.2 and
/// TLS 1.3 (`WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2`, `…_TLS1_3`).
const OPTION_SECURE_PROTOCOLS: u32 = 84;
const PROTOCOL_TLS1_2: u32 = 0x0000_0800;
const PROTOCOL_TLS1_3: u32 = 0x0000_2000;
/// `WINHTTP_OPTION_AUTOLOGON_POLICY` and its value
/// `WINHTTP_AUTOLOGON_SECURITY_LEVEL_HIGH`: never answer a request for
/// authentication with the account's own credentials.
const OPTION_AUTOLOGON_POLICY: u32 = 77;
const AUTOLOGON_NEVER: u32 = 2;
/// `WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER`: the
/// length the server announced for the body, as a number.
const QUERY_CONTENT_LENGTH_NUMBER: u32 = 0x05 | 0x2000_0000;
/// `WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER`: the status
/// as a number.
const QUERY_STATUS_CODE_NUMBER: u32 = 0x13 | 0x2000_0000;

/// The entry points of `winhttp.dll` this module calls, each with the
/// signature `winhttp.h` declares.
struct WinHttp {
    open: OpenFn,
    set_timeouts: SetTimeoutsFn,
    connect: ConnectFn,
    open_request: OpenRequestFn,
    set_option: SetOptionFn,
    send_request: SendRequestFn,
    receive_response: ReceiveResponseFn,
    query_headers: QueryHeadersFn,
    query_data_available: QueryDataAvailableFn,
    read_data: ReadDataFn,
    close_handle: CloseHandleFn,
}

/// [`WinHttp`], resolved once for the process.
fn winhttp() -> Result<&'static WinHttp, FetchError> {
    static WINHTTP: OnceLock<Option<WinHttp>> = OnceLock::new();
    WINHTTP
        .get_or_init(resolve_winhttp)
        .as_ref()
        .ok_or(FetchError::Unavailable)
}

fn resolve_winhttp() -> Option<WinHttp> {
    type Farproc = unsafe extern "system" fn() -> isize;
    // SAFETY: `LOAD_LIBRARY_SEARCH_SYSTEM32` confines the load to the
    // system directory. Each entry point is transmuted, from the
    // function pointer `GetProcAddress` returns, to the signature its
    // SDK header declares: the same size, and the only use made of it.
    unsafe {
        let dll = LoadLibraryExW(w!("winhttp.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
        Some(WinHttp {
            open: std::mem::transmute::<Farproc, OpenFn>(GetProcAddress(dll, s!("WinHttpOpen"))?),
            set_timeouts: std::mem::transmute::<Farproc, SetTimeoutsFn>(GetProcAddress(
                dll,
                s!("WinHttpSetTimeouts"),
            )?),
            connect: std::mem::transmute::<Farproc, ConnectFn>(GetProcAddress(
                dll,
                s!("WinHttpConnect"),
            )?),
            open_request: std::mem::transmute::<Farproc, OpenRequestFn>(GetProcAddress(
                dll,
                s!("WinHttpOpenRequest"),
            )?),
            set_option: std::mem::transmute::<Farproc, SetOptionFn>(GetProcAddress(
                dll,
                s!("WinHttpSetOption"),
            )?),
            send_request: std::mem::transmute::<Farproc, SendRequestFn>(GetProcAddress(
                dll,
                s!("WinHttpSendRequest"),
            )?),
            receive_response: std::mem::transmute::<Farproc, ReceiveResponseFn>(GetProcAddress(
                dll,
                s!("WinHttpReceiveResponse"),
            )?),
            query_headers: std::mem::transmute::<Farproc, QueryHeadersFn>(GetProcAddress(
                dll,
                s!("WinHttpQueryHeaders"),
            )?),
            query_data_available: std::mem::transmute::<Farproc, QueryDataAvailableFn>(
                GetProcAddress(dll, s!("WinHttpQueryDataAvailable"))?,
            ),
            read_data: std::mem::transmute::<Farproc, ReadDataFn>(GetProcAddress(
                dll,
                s!("WinHttpReadData"),
            )?),
            close_handle: std::mem::transmute::<Farproc, CloseHandleFn>(GetProcAddress(
                dll,
                s!("WinHttpCloseHandle"),
            )?),
        })
    }
}

/// A `WinHTTP` handle, closed when dropped.
struct Handle {
    raw: Hinternet,
    close: CloseHandleFn,
}

impl Handle {
    /// `raw` as a handle to close, or the error of the step that should
    /// have made it.
    fn new(raw: Hinternet, api: &WinHttp, step: &'static str) -> Result<Self, FetchError> {
        if raw.is_null() {
            return Err(last_error(step));
        }
        Ok(Self {
            raw,
            close: api.close_handle,
        })
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `raw` is a handle `WinHTTP` returned and this is the
        // only place that closes it.
        unsafe {
            (self.close)(self.raw);
        }
    }
}

/// The calling thread's last Windows error, as the failure of `step`.
fn last_error(step: &'static str) -> FetchError {
    // SAFETY: reads the calling thread's last-error value.
    FetchError::Request(step, unsafe { GetLastError() }.0)
}

/// `Ok` for a `BOOL` that is true, the thread's last error otherwise.
fn check(result: i32, step: &'static str) -> Result<(), FetchError> {
    if result == 0 {
        Err(last_error(step))
    } else {
        Ok(())
    }
}

/// `GET` `url` and return the body, which must be at most `max_bytes`
/// long.
///
/// Blocks for as long as the server takes, so it is for a worker
/// thread and never the UI thread (DESIGN.md §11.2). Each step of the
/// request (resolving, connecting, sending, the answer's first byte)
/// may take a quarter of a minute, and the body a minute in all and one
/// more wait, which bounds the whole at a little over two minutes.
/// The answer must be a `200`: any other status, a redirect included,
/// is an error. See the module's documentation for which URLs are
/// asked.
///
/// # Errors
///
/// [`FetchError`] says which step failed.
pub fn fetch(url: &str, user_agent: &str, max_bytes: usize) -> Result<Vec<u8>, FetchError> {
    fetch_within(url, user_agent, max_bytes, BODY_TIMEOUT)
}

/// [`fetch`], with the time the body may take as a parameter so that a
/// test reaches it.
fn fetch_within(
    url: &str,
    user_agent: &str,
    max_bytes: usize,
    body_timeout: Duration,
) -> Result<Vec<u8>, FetchError> {
    let target = parse_url(url)?;
    let api = winhttp()?;
    let agent = HSTRING::from(user_agent);
    let host = HSTRING::from(target.host.as_str());
    let object = HSTRING::from(target.object.as_str());
    // A local address is asked directly, whatever proxy the system has.
    let access = if target.host == "127.0.0.1" {
        ACCESS_TYPE_NO_PROXY
    } else {
        ACCESS_TYPE_AUTOMATIC_PROXY
    };

    // SAFETY: every pointer handed over is to a NUL-terminated wide
    // string, or a buffer with its true length, that outlives the call
    // it is passed to; every handle is one `WinHTTP` returned and has
    // not been closed, since `Handle` closes it only when dropped, in
    // the reverse of the order they were made.
    unsafe {
        let session = Handle::new(
            (api.open)(
                PCWSTR(agent.as_ptr()),
                access,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ),
            api,
            "open",
        )?;
        check(
            (api.set_timeouts)(
                session.raw,
                STEP_TIMEOUT_MS,
                STEP_TIMEOUT_MS,
                STEP_TIMEOUT_MS,
                STEP_TIMEOUT_MS,
            ),
            "set timeouts",
        )?;
        if target.secure {
            // TLS 1.2 and later, whatever older protocol the system
            // still has switched on. A system that does not know 1.3
            // refuses the pair, and is then asked for 1.2 alone.
            let modern = PROTOCOL_TLS1_2 | PROTOCOL_TLS1_3;
            let set = |protocols: u32| {
                (api.set_option)(
                    session.raw,
                    OPTION_SECURE_PROTOCOLS,
                    std::ptr::from_ref(&protocols).cast(),
                    DWORD_BYTES,
                )
            };
            if set(modern) == 0 {
                check(set(PROTOCOL_TLS1_2), "set protocols")?;
            }
        }
        let connection = Handle::new(
            (api.connect)(session.raw, PCWSTR(host.as_ptr()), target.port, 0),
            api,
            "connect",
        )?;
        let request = Handle::new(
            (api.open_request)(
                connection.raw,
                w!("GET"),
                PCWSTR(object.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                if target.secure { FLAG_SECURE } else { 0 },
            ),
            api,
            "open request",
        )?;
        let never = REDIRECT_POLICY_NEVER;
        check(
            (api.set_option)(
                request.raw,
                OPTION_REDIRECT_POLICY,
                std::ptr::from_ref(&never).cast(),
                DWORD_BYTES,
            ),
            "set redirect policy",
        )?;
        let never_log_on = AUTOLOGON_NEVER;
        check(
            (api.set_option)(
                request.raw,
                OPTION_AUTOLOGON_POLICY,
                std::ptr::from_ref(&never_log_on).cast(),
                DWORD_BYTES,
            ),
            "set logon policy",
        )?;
        check(
            (api.send_request)(request.raw, PCWSTR::null(), 0, std::ptr::null(), 0, 0, 0),
            "send",
        )?;
        check(
            (api.receive_response)(request.raw, std::ptr::null_mut()),
            "receive",
        )?;
        read_response(api, &request, max_bytes, Instant::now() + body_timeout)
    }
}

/// The body of the response `request` has received, which must be a
/// `200`, at most `max_bytes` long, as long as the server announced,
/// and all here by `deadline`.
///
/// # Safety
///
/// `request` is a request handle of `api`'s on which
/// `WinHttpReceiveResponse` has succeeded.
unsafe fn read_response(
    api: &WinHttp,
    request: &Handle,
    max_bytes: usize,
    deadline: Instant,
) -> Result<Vec<u8>, FetchError> {
    // SAFETY: the handle is open, by the caller's contract and
    // `Handle`'s; each out-pointer is to a local of the type the call
    // writes, and the buffers are passed with their true lengths.
    unsafe {
        let mut status = 0u32;
        let mut status_len = DWORD_BYTES;
        check(
            (api.query_headers)(
                request.raw,
                QUERY_STATUS_CODE_NUMBER,
                PCWSTR::null(),
                std::ptr::from_mut(&mut status).cast(),
                &raw mut status_len,
                std::ptr::null_mut(),
            ),
            "read status",
        )?;
        if status != 200 {
            return Err(FetchError::Status(status));
        }
        // The length the server announced, if it announced one: a body
        // sent in chunks has none. `WinHTTP` reports a connection the
        // server closed early as the body's end (measured), so without
        // this a feed cut short would pass for a whole one.
        let mut announced = 0u32;
        let mut announced_len = DWORD_BYTES;
        let announced = ((api.query_headers)(
            request.raw,
            QUERY_CONTENT_LENGTH_NUMBER,
            PCWSTR::null(),
            std::ptr::from_mut(&mut announced).cast(),
            &raw mut announced_len,
            std::ptr::null_mut(),
        ) != 0)
            .then_some(announced as usize);
        if announced.is_some_and(|announced| announced > max_bytes) {
            return Err(FetchError::TooLarge);
        }
        let whole = |body: Vec<u8>| match announced {
            Some(announced) if body.len() != announced => Err(FetchError::CutShort),
            _ => Ok(body),
        };

        let mut body = Vec::new();
        let mut chunk = [0u8; CHUNK_BYTES as usize];
        loop {
            if Instant::now() >= deadline {
                return Err(FetchError::TooSlow);
            }
            // Ask what has arrived and read no more than that. A read
            // of a whole chunk waits until the chunk is full or the
            // body ends, however slowly it comes (measured: 50 s for a
            // kilobyte sent a byte at a time), and the deadline above
            // would be looked at once, at the end.
            let mut available = 0u32;
            check(
                (api.query_data_available)(request.raw, &raw mut available),
                "wait for data",
            )?;
            if available == 0 {
                return whole(body);
            }
            let mut read = 0u32;
            check(
                (api.read_data)(
                    request.raw,
                    chunk.as_mut_ptr().cast(),
                    available.min(CHUNK_BYTES),
                    &raw mut read,
                ),
                "read",
            )?;
            if read == 0 {
                return whole(body);
            }
            let read = (read as usize).min(chunk.len());
            if body.len() + read > max_bytes {
                return Err(FetchError::TooLarge);
            }
            body.extend_from_slice(&chunk[..read]);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn a_url_is_taken_apart() {
        assert_eq!(
            parse_url("https://git.example.org/tux/code/raw/branch/appcast/win.xml"),
            Ok(Target {
                secure: true,
                host: "git.example.org".into(),
                port: 443,
                object: "/tux/code/raw/branch/appcast/win.xml".into(),
            })
        );
        assert_eq!(
            parse_url("https://example.org:8443/a?b=c"),
            Ok(Target {
                secure: true,
                host: "example.org".into(),
                port: 8443,
                object: "/a?b=c".into(),
            })
        );
        assert_eq!(
            parse_url("https://example.org"),
            Ok(Target {
                secure: true,
                host: "example.org".into(),
                port: 443,
                object: "/".into(),
            })
        );
        assert_eq!(
            parse_url("http://127.0.0.1:8080/feed.xml"),
            Ok(Target {
                secure: false,
                host: "127.0.0.1".into(),
                port: 8080,
                object: "/feed.xml".into(),
            })
        );
    }

    #[test]
    fn a_url_that_is_not_a_plain_https_address_is_refused() {
        let long = format!("https://example.org/{}", "a".repeat(MAX_URL_BYTES));
        for url in [
            "",
            "example.org/feed.xml",
            "ftp://example.org/feed.xml",
            "file:///C:/feed.xml",
            "HTTPS://example.org/feed.xml",
            // Plain http, to anything but this machine.
            "http://example.org/feed.xml",
            "http://localhost/feed.xml",
            "http://127.0.0.1.example.org/feed.xml",
            "http://127.0.0.2/feed.xml",
            // User information, which would put the real host after an @.
            "https://example.org@evil.example/feed.xml",
            "https://user:pw@example.org/feed.xml",
            "http://127.0.0.1@evil.example/feed.xml",
            "http://127.0.0.1:80@evil.example/feed.xml",
            // Hosts that are not plain names.
            "https:///feed.xml",
            "https://exa mple.org/feed.xml",
            "https://[::1]/feed.xml",
            "https://.example.org/feed.xml",
            "https://example.org./feed.xml",
            "https://-example.org/feed.xml",
            "https://ex\u{e4}mple.org/feed.xml",
            "https://example.org\\@evil.example/feed.xml",
            "https://example..org/feed.xml",
            // Ports that are not port numbers.
            "https://example.org:/feed.xml",
            "https://example.org:0/feed.xml",
            "https://example.org:65536/feed.xml",
            "https://example.org:+443/feed.xml",
            "https://example.org:443x/feed.xml",
            // Paths with something Windows or a server could read otherwise.
            "https://example.org/feed.xml#frag",
            "https://example.org/a b",
            "https://example.org/a\\b",
            "https://example.org/..\\feed.xml",
            "https://example.org/a\r\nHost: evil.example",
            "https://example.org/a\0b",
            "https://example.org/\u{202e}",
            long.as_str(),
        ] {
            assert!(
                matches!(parse_url(url), Err(FetchError::Url(_))),
                "{url:?} was taken"
            );
        }
    }

    /// Serve `responses`, one per connection, on a local port, and give
    /// the port. Each connection is read up to the end of its request's
    /// headers, answered and closed.
    fn serve(responses: Vec<Vec<u8>>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => request.push(byte[0]),
                        _ => break,
                    }
                }
                let _ = stream.write_all(&response);
            }
        });
        port
    }

    fn ok_response(body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[test]
    fn a_feed_is_fetched_whole_from_a_local_server() {
        let body: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let port = serve(vec![ok_response(&body)]);
        let got = fetch(
            &format!("http://127.0.0.1:{port}/feed.xml"),
            "Code++ test",
            body.len(),
        );
        assert_eq!(got, Ok(body));
    }

    #[test]
    fn a_body_over_the_cap_is_refused() {
        let body = vec![b'x'; 40_000];
        let port = serve(vec![ok_response(&body)]);
        let got = fetch(
            &format!("http://127.0.0.1:{port}/feed.xml"),
            "Code++ test",
            body.len() - 1,
        );
        assert_eq!(got, Err(FetchError::TooLarge));
    }

    #[test]
    fn a_status_other_than_200_is_an_error_and_a_redirect_is_not_followed() {
        // The redirect names a second server, which would answer 200:
        // followed, the fetch would succeed.
        let elsewhere = serve(vec![ok_response(b"followed")]);
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{elsewhere}/feed.xml\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let missing = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let port = serve(vec![redirect.into_bytes(), missing.into()]);
        let url = format!("http://127.0.0.1:{port}/feed.xml");
        assert_eq!(
            fetch(&url, "Code++ test", 1024),
            Err(FetchError::Status(302))
        );
        assert_eq!(
            fetch(&url, "Code++ test", 1024),
            Err(FetchError::Status(404))
        );
    }

    /// The TLS half, which no local server here can stand in for. Opt-in,
    /// since it leaves the machine: `CODEPP_FEED_TEST_URL` names an
    /// `https:` URL that answers 200, and `CODEPP_FEED_TEST_BAD_TLS_URLS`
    /// one or more, separated by spaces, whose certificate is not to be
    /// trusted (expired, for another host, self-signed).
    #[test]
    #[ignore = "makes requests to the URLs in CODEPP_FEED_TEST_URL and CODEPP_FEED_TEST_BAD_TLS_URLS"]
    fn https_is_fetched_and_an_untrusted_certificate_ends_the_request() {
        let good = std::env::var("CODEPP_FEED_TEST_URL").expect("CODEPP_FEED_TEST_URL");
        let body = fetch(&good, "Code++ test", 1024 * 1024).expect("the https fetch");
        assert!(!body.is_empty());
        let bad = std::env::var("CODEPP_FEED_TEST_BAD_TLS_URLS").unwrap_or_default();
        for url in bad.split_whitespace() {
            let got = fetch(url, "Code++ test", 1024 * 1024);
            assert!(
                matches!(got, Err(FetchError::Request("send", code)) if code != 0),
                "{url}: {got:?}"
            );
        }
    }

    #[test]
    fn a_body_that_dribbles_in_is_given_up_on() {
        // The server sends a byte every 50 ms, which no step timeout
        // would ever catch.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(1) => request.push(byte[0]),
                    _ => return,
                }
            }
            let head = "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n";
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            for _ in 0..1000 {
                if stream.write_all(b"x").is_err() || stream.flush().is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let before = Instant::now();
        let got = fetch_within(
            &format!("http://127.0.0.1:{port}/feed.xml"),
            "Code++ test",
            1024 * 1024,
            Duration::from_millis(400),
        );
        assert_eq!(got, Err(FetchError::TooSlow));
        // Given up on when the time is up, and not when the last byte
        // has arrived, fifty seconds in.
        assert!(
            before.elapsed() < Duration::from_secs(10),
            "{:?}",
            before.elapsed()
        );
    }

    #[test]
    fn a_body_cut_short_is_an_error_and_not_a_short_feed() {
        // The server promises a thousand bytes, sends ten and hangs up.
        let mut response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n".to_vec();
        response.extend_from_slice(b"0123456789");
        let port = serve(vec![response]);
        let got = fetch(
            &format!("http://127.0.0.1:{port}/feed.xml"),
            "Code++ test",
            1024 * 1024,
        );
        assert_eq!(got, Err(FetchError::CutShort));
    }

    #[test]
    fn a_server_that_is_not_there_is_an_error() {
        // Bind a port and close it again, so that nothing listens there.
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let got = fetch(
            &format!("http://127.0.0.1:{port}/feed.xml"),
            "Code++ test",
            1024,
        );
        assert!(
            matches!(got, Err(FetchError::Request(_, code)) if code != 0),
            "{got:?}"
        );
    }
}
