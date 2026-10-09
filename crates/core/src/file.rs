//! Async file loading on a dedicated worker thread.
//!
//! See DESIGN.md §5.4 (cross-thread UI marshaling) and §7.2 Phase 2.
//!
//! The loader owns a single worker thread that drains an unbounded
//! request channel and posts results to an unbounded result channel.
//! UI code (the `shell` crate) holds a [`Loader`] handle, queues opens
//! via [`Loader::open`], and pulls results from the receiver — typically
//! after being woken by a Win32 `WM_APP+x` posted by the worker. Phase 2
//! ships a one-thread loader; Phase 4's find-in-files moves to a thread
//! pool when concurrent reads matter.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::encoding::{self, Encoding, EncodingError};
use crate::eol::{self, Eol};

/// Opaque identifier for a load request, returned by [`Loader::open`]
/// and echoed back in [`LoadResult`]. Lets the UI correlate a result
/// with the originating action (e.g., dimming a tab while it loads).
pub type RequestId = u64;

/// A successfully loaded file.
#[derive(Debug, Clone)]
pub struct LoadedFile {
    pub id: RequestId,
    pub path: PathBuf,
    /// File contents decoded into a Rust string, using the detected
    /// encoding. The original byte length is captured separately so
    /// the status bar can show "1.2 MB / 1,034,567 chars" without a
    /// second `metadata()` call.
    pub text: String,
    pub encoding: Encoding,
    pub eol: Eol,
    pub byte_len: u64,
}

/// A load attempt that didn't produce a usable buffer.
#[derive(Debug)]
pub struct LoadError {
    pub id: RequestId,
    pub path: PathBuf,
    pub error: LoadErrorKind,
}

/// Cause of a [`LoadError`]. Surfaced to the UI verbatim — the UI
/// converts to a localized message at the boundary.
#[derive(Debug)]
pub enum LoadErrorKind {
    /// `std::io` error reading the file (not found, permission denied,
    /// disk error).
    Io(io::Error),
    /// File opened cleanly but its bytes don't decode under the
    /// detected encoding.
    Encoding(EncodingError),
    /// File exceeds [`MAX_FILE_BYTES`]. Code++ refuses to load
    /// multi-gigabyte files into a single in-memory buffer; users who
    /// need to inspect such files should use a tool designed for them.
    /// `actual_bytes` is the length the file reported, or one past the cap
    /// for a file that held more than it reported.
    TooLarge { actual_bytes: u64, max_bytes: u64 },
    /// The path names a folder, a named pipe, a socket or a device rather
    /// than a regular file, as the loader's opener ([`OpenRegularFile`]) or
    /// its own check on what the opener hands back tells it. Reading a
    /// named pipe waits for something to write to it, and reading a device
    /// like `/dev/zero` never ends. Either would hold the loader's one
    /// worker, and every open queued behind it, for good.
    NotARegularFile,
}

/// Maximum file size the loader will read into memory in one shot.
/// 512 MiB is large enough for any plausible text file and small enough
/// that the worker can't OOM the process on a typical desktop. Phase 4's
/// thread-pool variant may revisit this when concurrent reads land.
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

impl std::fmt::Display for LoadErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadErrorKind::Io(e) => write!(f, "I/O error: {e}"),
            LoadErrorKind::Encoding(e) => write!(f, "{e}"),
            LoadErrorKind::TooLarge {
                actual_bytes,
                max_bytes,
            } => write!(
                f,
                "file is {actual_bytes} bytes; Code++ refuses files larger than {max_bytes} bytes"
            ),
            LoadErrorKind::NotARegularFile => write!(f, "not a regular file"),
        }
    }
}

impl std::error::Error for LoadErrorKind {}

/// Result delivered on the loader's result channel.
pub type LoadResult = Result<LoadedFile, LoadError>;

/// How the loader opens a path: the file if the path names a regular file,
/// and `Ok(None)` if it names anything else, which the loader refuses
/// without reading ([`LoadErrorKind::NotARegularFile`]).
///
/// An opener must not wait on what the path names. On Unix even *opening*
/// a named pipe for reading waits for a writer, and that would hold the
/// loader's one worker, and every open queued behind it, for good. Not
/// waiting takes the platform's own open flags, which this crate leaves to
/// the platform layer: the shell passes `codepp_platform::open_regular_file`.
pub type OpenRegularFile = fn(&Path) -> io::Result<Option<File>>;

/// Internal request frame.
enum Request {
    Open { id: RequestId, path: PathBuf },
    Shutdown,
}

/// Handle to the loader's worker thread. Cloneable — multiple UI sites
/// can share one loader.
#[derive(Clone)]
pub struct Loader {
    next_id: Arc<AtomicU64>,
    sender: Sender<Request>,
}

/// Owning handle to the worker thread. Held by the application root;
/// dropping it sends the shutdown sentinel and joins the worker.
///
/// **Lifetime contract:** the worker lives until `LoaderShutdown` is
/// dropped. Dropping every `Loader` clone alone does **not** stop the
/// worker — `Loader` and `Receiver` are non-owning consumers. After
/// `LoaderShutdown` is dropped, calling `Loader::open` on any surviving
/// `Loader` clone returns `None` and the request is silently
/// discarded; callers must treat that `None` as "the editor is shutting
/// down, abandon the action".
pub struct LoaderShutdown {
    sender: Sender<Request>,
    join: Option<JoinHandle<()>>,
}

impl Drop for LoaderShutdown {
    fn drop(&mut self) {
        // Best-effort shutdown: send the sentinel and wait for the
        // worker to finish whatever it has in flight. If the channel is
        // already closed, the worker has exited; if the join handle was
        // already taken, do nothing.
        let _ = self.sender.send(Request::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Loader {
    /// Spawn the worker thread, which opens each path with `open`, and
    /// return three handles:
    ///   - the cloneable `Loader` for queuing reads,
    ///   - the result `Receiver` for the UI thread to drain,
    ///   - a `LoaderShutdown` whose drop joins the worker.
    ///
    /// # Panics
    ///
    /// Panics if `thread::Builder::spawn` fails (out of memory or
    /// an OS-side thread-creation refusal). Surfacing this as a
    /// panic matches the "first call at app startup, can't proceed
    /// without it" gate the loader represents.
    #[must_use]
    pub fn spawn(open: OpenRegularFile) -> (Loader, Receiver<LoadResult>, LoaderShutdown) {
        let (req_tx, req_rx) = unbounded::<Request>();
        let (res_tx, res_rx) = unbounded::<LoadResult>();

        let join = thread::Builder::new()
            .name("codepp-file-loader".into())
            .spawn(move || worker_loop(&req_rx, &res_tx, open))
            .expect("file loader thread spawn");

        let next_id = Arc::new(AtomicU64::new(1));
        let loader = Loader {
            next_id,
            sender: req_tx.clone(),
        };
        let shutdown = LoaderShutdown {
            sender: req_tx,
            join: Some(join),
        };
        (loader, res_rx, shutdown)
    }

    /// Queue a file open. Returns the request id; the matching result
    /// will arrive on the receiver once the worker reads and decodes
    /// the file. Returns `None` if the worker has already shut down
    /// (channel closed).
    pub fn open(&self, path: impl Into<PathBuf>) -> Option<RequestId> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.sender
            .send(Request::Open {
                id,
                path: path.into(),
            })
            .ok()?;
        Some(id)
    }
}

fn worker_loop(req_rx: &Receiver<Request>, res_tx: &Sender<LoadResult>, open: OpenRegularFile) {
    while let Ok(req) = req_rx.recv() {
        match req {
            Request::Shutdown => break,
            Request::Open { id, path } => {
                let result = read_and_decode(id, &path, open);
                if res_tx.send(result).is_err() {
                    // UI thread dropped the receiver — shut down
                    // immediately rather than processing pending
                    // requests no one will read.
                    break;
                }
            }
        }
    }
    tracing::debug!("file loader thread exiting");
}

fn read_and_decode(id: RequestId, path: &Path, open: OpenRegularFile) -> LoadResult {
    read_and_decode_within(id, path, open, MAX_FILE_BYTES)
}

/// [`read_and_decode`] with the cap passed in, so that the tests reach each
/// size check with a file of a few bytes.
fn read_and_decode_within(
    id: RequestId,
    path: &Path,
    open: OpenRegularFile,
    cap: u64,
) -> LoadResult {
    let fail = |error| {
        Err(LoadError {
            id,
            path: path.to_path_buf(),
            error,
        })
    };
    let too_large = |actual_bytes| {
        fail(LoadErrorKind::TooLarge {
            actual_bytes,
            max_bytes: cap,
        })
    };
    // A file over the cap is refused before it is opened, since opening a
    // file can cost more than looking it up: a virus scanner may scan it,
    // and a cloud-storage client may fetch it. What this lookup does not
    // settle, a path that changes after it included, is left to the checks
    // on the opened file below.
    if let Ok(metadata) = std::fs::metadata(path) {
        if metadata.is_file() && metadata.len() > cap {
            return too_large(metadata.len());
        }
    }
    // Only a regular file is read ([`LoadErrorKind::NotARegularFile`]).
    // The opener checks, and so does the type check below, on the opened
    // file: `fstat` on its descriptor rather than a fresh lookup by name,
    // so the path cannot be swapped for a pipe between the check and the
    // read whatever the opener does.
    let mut file = match open(path) {
        Ok(Some(f)) => f,
        Ok(None) => return fail(LoadErrorKind::NotARegularFile),
        Err(e) => return fail(LoadErrorKind::Io(e)),
    };
    let metadata = match file.metadata() {
        Ok(m) => m,
        Err(e) => return fail(LoadErrorKind::Io(e)),
    };
    if !metadata.is_file() {
        return fail(LoadErrorKind::NotARegularFile);
    }
    // Check the size of the file opened, too, so that we never allocate
    // gigabytes for a single path, even one that grew or was swapped after
    // the lookup above.
    let reported = metadata.len();
    if reported > cap {
        return too_large(reported);
    }
    let bytes = match read_at_most(&mut file, reported, cap) {
        Ok(Some(b)) => b,
        Ok(None) => return too_large(cap.saturating_add(1)),
        Err(e) => return fail(LoadErrorKind::Io(e)),
    };
    let byte_len = bytes.len() as u64;

    let (encoding, body) = encoding::detect(&bytes);
    let text = match encoding::decode(body, &encoding) {
        Ok(t) => t,
        Err(e) => return fail(LoadErrorKind::Encoding(e)),
    };

    // EOL detection runs on the *decoded* text, not the raw bytes. For
    // UTF-16 files, raw bytes interleave NULs with the line endings, so
    // a raw-byte scan misses them entirely. The decoded text is what
    // Scintilla sees, and that's the right surface for EOL.
    let eol = eol::detect(text.as_bytes());

    Ok(LoadedFile {
        id,
        path: path.to_path_buf(),
        text,
        encoding,
        eol,
        byte_len,
    })
}

/// Read all of `reader` if it holds at most `cap` bytes, and `Ok(None)` if
/// it holds more. `len`, the length the file reported, sizes the buffer,
/// fallibly: a failed allocation is an error to report, where
/// `Vec::with_capacity` would abort the process. The read is bounded by
/// `cap` rather than by `len`, since a file can grow after its length was
/// read, or report less than it holds (`/proc` reports 0).
fn read_at_most(reader: impl Read, len: u64, cap: u64) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    let size = usize::try_from(len.min(cap)).unwrap_or(usize::MAX);
    bytes
        .try_reserve_exact(size)
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
    reader.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= cap).then_some(bytes))
}

#[cfg(test)]
// Tests bind `loader` and `loaded` (the LoadResult) in close
// proximity — semantically distinct but clippy's pedantic
// `similar_names` lint flags them as a typo hazard. Allowed at
// module level so we don't have to invent uglier names just to
// satisfy the lint in test scaffolding.
#[allow(clippy::similar_names)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{Duration, Instant};

    fn write_temp(contents: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents).unwrap();
        f.flush().unwrap();
        f
    }

    /// An opener using `std` alone, for the tests here, which open only
    /// regular files and folders: its open would wait on a named pipe,
    /// which the platform's opener does not (`codepp_platform`'s tests).
    fn std_open(path: &Path) -> io::Result<Option<File>> {
        let file = File::open(path)?;
        Ok(file.metadata()?.is_file().then_some(file))
    }

    #[test]
    fn loads_utf8_lf_file() {
        let f = write_temp(b"hello\nworld\n");
        let (loader, results, _shutdown) = Loader::spawn(std_open);
        let id = loader.open(f.path()).unwrap();

        let result = results.recv_timeout(Duration::from_secs(2)).unwrap();
        let loaded = result.expect("load should succeed");

        assert_eq!(loaded.id, id);
        assert_eq!(loaded.text, "hello\nworld\n");
        assert_eq!(loaded.encoding, Encoding::Utf8);
        assert_eq!(loaded.eol, Eol::Lf);
        assert_eq!(loaded.byte_len, 12);
    }

    #[test]
    fn loads_utf8_bom_crlf_file() {
        let mut bytes = Vec::from(b"\xEF\xBB\xBF" as &[u8]);
        bytes.extend_from_slice(b"line1\r\nline2\r\n");
        let f = write_temp(&bytes);
        let (loader, results, _shutdown) = Loader::spawn(std_open);
        loader.open(f.path()).unwrap();

        let loaded = results
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.text, "line1\r\nline2\r\n");
        assert_eq!(loaded.encoding, Encoding::Utf8Bom);
        assert_eq!(loaded.eol, Eol::CrLf);
    }

    #[test]
    fn loads_utf16_le_bom_file() {
        let mut bytes = Vec::from(b"\xFF\xFE" as &[u8]);
        for u in "hello".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let f = write_temp(&bytes);
        let (loader, results, _shutdown) = Loader::spawn(std_open);
        loader.open(f.path()).unwrap();

        let loaded = results
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.text, "hello");
        assert_eq!(loaded.encoding, Encoding::Utf16LeBom);
    }

    #[test]
    fn missing_file_yields_io_error() {
        let (loader, results, _shutdown) = Loader::spawn(std_open);
        loader
            .open("definitely-nonexistent-path-12345.txt")
            .unwrap();

        let result = results.recv_timeout(Duration::from_secs(2)).unwrap();
        let err = result.expect_err("missing file should fail");
        assert!(matches!(err.error, LoadErrorKind::Io(_)));
    }

    #[test]
    fn malformed_utf8_yields_encoding_error() {
        // Lone 0xC0 — invalid UTF-8 start byte. Detection falls back to
        // Windows-1252, which DOES accept these bytes — so this file
        // would actually load successfully under Windows-1252. To force
        // an encoding error we instead write a UTF-8 BOM followed by
        // invalid UTF-8 bytes; the BOM forces UTF-8 decoding, and the
        // bytes then fail strict-decode.
        let mut bytes = Vec::from(b"\xEF\xBB\xBF" as &[u8]);
        bytes.extend_from_slice(b"valid then \xC3\x28 broken");
        let f = write_temp(&bytes);
        let (loader, results, _shutdown) = Loader::spawn(std_open);
        loader.open(f.path()).unwrap();

        let result = results.recv_timeout(Duration::from_secs(2)).unwrap();
        let err = result.expect_err("malformed utf-8 should fail");
        assert!(matches!(err.error, LoadErrorKind::Encoding(_)));
    }

    #[test]
    fn many_concurrent_opens_all_resolve() {
        let mut files = Vec::new();
        for i in 0..16 {
            let f = write_temp(format!("file {i}\n").as_bytes());
            files.push(f);
        }
        let (loader, results, _shutdown) = Loader::spawn(std_open);
        let mut ids = Vec::new();
        for f in &files {
            ids.push(loader.open(f.path()).unwrap());
        }

        // Single absolute deadline — earlier versions of this test used a
        // per-recv 2-second timeout and accumulated 32 s wall time on a
        // slow CI machine, producing flake. 5 s across all 16 reads is
        // generous; failure here means real loader breakage.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = std::collections::HashSet::new();
        for _ in 0..files.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let r = results.recv_timeout(remaining).unwrap().unwrap();
            received.insert(r.id);
        }
        for id in ids {
            assert!(received.contains(&id), "missing result for id {id}");
        }
    }

    /// A file over the cap is refused before it is opened: its opener is
    /// never asked.
    #[test]
    fn a_file_over_the_cap_is_refused_before_it_is_opened() {
        let f = write_temp(b"12345");
        let err = read_and_decode_within(7, f.path(), |_| Err(io::Error::other("opened")), 4)
            .expect_err("over the cap");
        assert!(
            matches!(
                err.error,
                LoadErrorKind::TooLarge {
                    actual_bytes: 5,
                    max_bytes: 4
                }
            ),
            "{:?}",
            err.error
        );
        assert_eq!(
            err.error.to_string(),
            "file is 5 bytes; Code++ refuses files larger than 4 bytes"
        );
    }

    /// A file of exactly the cap is read: each size check refuses only a
    /// file over it.
    #[test]
    fn a_file_of_exactly_the_cap_is_read() {
        let f = write_temp(b"1234");
        let loaded = read_and_decode_within(7, f.path(), std_open, 4).expect("at the cap");
        assert_eq!(loaded.text, "1234");
        assert_eq!(loaded.byte_len, 4);
    }

    /// The loader's own cap is [`MAX_FILE_BYTES`]: a file one byte over it is
    /// refused before it is opened. Sparse, so nothing is written.
    #[cfg(unix)]
    #[test]
    fn the_loader_refuses_a_file_one_byte_over_max_file_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        File::create(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let (loader, results, _shutdown) = Loader::spawn(|_| Err(io::Error::other("opened")));
        loader.open(&path).unwrap();
        let err = results
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .expect_err("over the cap");
        assert!(
            matches!(
                err.error,
                LoadErrorKind::TooLarge { actual_bytes, max_bytes }
                    if actual_bytes == MAX_FILE_BYTES + 1 && max_bytes == MAX_FILE_BYTES
            ),
            "{:?}",
            err.error
        );
    }

    /// The size checked before the open is that of the file a link names,
    /// not the link's own.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_file_over_the_cap_is_refused_before_it_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("big");
        std::fs::write(&target, b"12345").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = read_and_decode_within(7, &link, |_| Err(io::Error::other("opened")), 4)
            .expect_err("over the cap");
        assert!(
            matches!(
                err.error,
                LoadErrorKind::TooLarge {
                    actual_bytes: 5,
                    max_bytes: 4
                }
            ),
            "{:?}",
            err.error
        );
    }

    /// A file that is over the cap when it is opened, though the path was
    /// not when it was looked up, is refused without being read: the size
    /// of the file opened is checked again. The opener here opens another
    /// file than the one looked up, as a path swapped between the two does.
    #[test]
    fn a_file_over_the_cap_when_opened_is_refused_unread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p");
        std::fs::write(&path, b"12").unwrap();
        std::fs::write(path.with_extension("big"), b"123456").unwrap();
        let err = read_and_decode_within(7, &path, |p| std_open(&p.with_extension("big")), 4)
            .expect_err("over the cap when opened");
        // Read, it would be reported as one byte over the cap.
        assert!(
            matches!(
                err.error,
                LoadErrorKind::TooLarge {
                    actual_bytes: 6,
                    max_bytes: 4
                }
            ),
            "{:?}",
            err.error
        );
    }

    /// A file that holds more than it reports is refused once the read
    /// passes the cap, and reported as one byte over it. `/proc` files
    /// report no length; where there is no `/proc/version`, as off Linux,
    /// there is nothing to test.
    #[test]
    fn a_file_that_holds_more_than_it_reports_is_refused() {
        if !Path::new("/proc/version").exists() {
            return;
        }
        let err = read_and_decode_within(7, Path::new("/proc/version"), std_open, 4)
            .expect_err("/proc/version holds more than 4 bytes");
        assert!(
            matches!(
                err.error,
                LoadErrorKind::TooLarge {
                    actual_bytes: 5,
                    max_bytes: 4
                }
            ),
            "{:?}",
            err.error
        );
    }

    /// A file that reports no length is read whole, up to the cap: `/proc`
    /// files report none and hold text. Off Linux there is no `/proc/version`
    /// to read.
    #[test]
    fn a_file_that_reports_no_length_is_read_whole() {
        if !Path::new("/proc/version").exists() {
            return;
        }
        let (loader, results, _shutdown) = Loader::spawn(std_open);
        loader.open("/proc/version").unwrap();
        let loaded = results
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .expect("/proc/version loads");
        assert!(loaded.text.starts_with("Linux version"), "{}", loaded.text);
        assert_eq!(loaded.byte_len, loaded.text.len() as u64);
    }

    /// What the opener refuses is refused without being read, and the open
    /// after it lands.
    #[test]
    fn what_the_opener_refuses_is_not_read() {
        let after = write_temp(b"after\n");
        let refuse = after.path().with_extension("refused");
        let (loader, results, _shutdown) = Loader::spawn(|path| {
            if path.extension().is_some_and(|e| e == "refused") {
                Ok(None)
            } else {
                std_open(path)
            }
        });
        loader.open(&refuse).unwrap();
        loader.open(after.path()).unwrap();
        let err = results
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .expect_err("refused by the opener");
        assert!(matches!(err.error, LoadErrorKind::NotARegularFile));
        assert_eq!(err.error.to_string(), "not a regular file");
        let second = results
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(second.text, "after\n");
    }

    /// The loader checks the type of what an opener hands back itself, so a
    /// folder an opener lets through is still refused rather than read.
    #[cfg(unix)]
    #[test]
    fn what_an_opener_hands_back_is_checked_again() {
        let dir = tempfile::tempdir().unwrap();
        let (loader, results, _shutdown) = Loader::spawn(|path| File::open(path).map(Some));
        loader.open(dir.path()).unwrap();
        let err = results
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .expect_err("a folder is not a file");
        assert!(matches!(err.error, LoadErrorKind::NotARegularFile));
    }

    /// The read is bounded by the cap, not by the length the file reported:
    /// a file that holds more than it reported, as one that grew after its
    /// length was read does, is refused rather than read in full, and one
    /// that reports nothing but holds text (`/proc`) is read whole.
    #[test]
    fn the_read_is_bounded_by_the_cap_not_the_reported_length() {
        let read = |bytes: &[u8], len: u64| read_at_most(bytes, len, 4).unwrap();
        assert_eq!(read(b"abc", 3).as_deref(), Some(&b"abc"[..]));
        assert_eq!(read(b"abcd", 4).as_deref(), Some(&b"abcd"[..]));
        assert_eq!(read(b"abcde", 2), None);
        assert_eq!(read(b"abcdefgh", 0), None);
        assert_eq!(read(b"abc", 0).as_deref(), Some(&b"abc"[..]));
        assert_eq!(read(b"", 9).as_deref(), Some(&b""[..]));
    }

    /// The buffer is reserved fallibly: a length no allocation can hold is
    /// an error to report, where an infallible reservation would abort.
    #[test]
    fn a_reservation_that_cannot_be_made_is_an_error() {
        let err = read_at_most(&b""[..], u64::MAX, u64::MAX).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
    }

    /// The buffer is sized by the reported length only up to the cap, so a
    /// file that reports more than it holds costs no more than the cap.
    #[test]
    fn the_reservation_never_exceeds_the_cap() {
        let got = read_at_most(&b""[..], u64::MAX, 4).unwrap();
        assert_eq!(got.as_deref(), Some(&b""[..]));
    }

    #[test]
    fn eol_detection_runs_on_decoded_text_for_utf16() {
        // Regression for a Phase 2 review finding: EOL detection used to
        // run on the raw bytes, so a UTF-16 LE file with CRLF endings
        // (where raw bytes are `0D 00 0A 00`) would report Lf because
        // the scanner saw `0D 00` not `0D 0A` consecutively. Running on
        // the decoded text fixes this.
        let mut bytes = Vec::from(b"\xFF\xFE" as &[u8]);
        for u in "line1\r\nline2\r\n".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let f = write_temp(&bytes);
        let (loader, results, _shutdown) = Loader::spawn(std_open);
        loader.open(f.path()).unwrap();

        let loaded = results
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(loaded.encoding, Encoding::Utf16LeBom);
        assert_eq!(loaded.eol, Eol::CrLf);
    }

    #[test]
    fn shutdown_joins_cleanly() {
        let (_loader, _results, shutdown) = Loader::spawn(std_open);
        // Dropping the LoaderShutdown sends the Shutdown sentinel and
        // joins the thread. If this hangs, the worker isn't honouring
        // the sentinel.
        drop(shutdown);
    }
}
