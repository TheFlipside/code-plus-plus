//! The auto-updater's worker on Windows (DESIGN.md §11.3, §11.4).
//!
//! Everything the updater does happens on one thread of its own, which
//! the UI starts after the first frame is drawn and never waits for.
//! The thread
//!
//! 1. establishes whether this copy of Code++ can update itself at all,
//!    and tells the UI, which until then keeps "Update Code++" and the
//!    Auto-updater preference greyed (§11.5);
//! 2. decides, at its start and every hour after, whether a check is
//!    due (`codepp_core::update::check_is_due`), and only then loads the
//!    updater library and asks it to check;
//! 3. runs a check with the library's window when the user asks for one
//!    from the menu, whatever the preference says;
//! 4. takes the installer the library downloaded, verifies it against
//!    the feed's signature itself, and hands the verified file to the
//!    UI, which quits and starts Windows Installer as its last act.
//!
//! The UI thread sends [`Updater`] commands through a channel and reads
//! [`UpdaterEvent`]s from another on a wake, the marshaling rule of
//! DESIGN.md §5.4. It never calls the library, opens the feed or reads
//! the state file.
//!
//! **A copy that can update itself** is one whose build was given a
//! feed and a public key, that Windows Installer put where it is, of a
//! kind that may update (§11.4), with the library's DLL beside it. Any
//! other copy has no updater: a build tree, the zip, an installation
//! for the whole machine while those are gated.
//!
//! **The feed's URL and the public key are constants of the build**,
//! `CODEPP_UPDATE_FEED_URL` and `CODEPP_UPDATE_PUBLIC_KEY` in its
//! environment (§11.7). Neither is ever read from a file, a setting or
//! the registry, so nothing on the machine can point an installed copy
//! at another feed or have it trust another key.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use codepp_core::update::{self, feed, UpdaterState};
use codepp_platform::update_feed::{self, FetchError};
use codepp_platform::update_install::{
    self, InstallError, InstallKind, VerifiedInstaller, PER_MACHINE_UPDATES,
};
use codepp_platform::winsparkle::{self, ConfigStore, WinSparkle};
use crossbeam_channel::{Receiver, Sender};

/// The feed's URL, if the build was given one.
const BUILD_FEED_URL: Option<&str> = option_env!("CODEPP_UPDATE_FEED_URL");

/// The Ed25519 public key updates are verified against, as base64, if
/// the build was given one.
const BUILD_PUBLIC_KEY: Option<&str> = option_env!("CODEPP_UPDATE_PUBLIC_KEY");

/// How often the worker asks again whether a check is due, for a
/// session that stays open past the interval.
const RECHECK_EVERY: Duration = Duration::from_hours(1);

/// How long a quit waits for the worker to stop the library before it
/// goes on without it.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

/// How long a scheduled check may go without an answer before the
/// worker stops waiting for it. The library answers a check within
/// seconds or reports an error; one it never answers must not keep
/// every later check from starting for the rest of the session.
const CHECK_GIVEN_UP_AFTER: Duration = Duration::from_mins(10);

/// What the build knows of its updates: where the feed is and which key
/// signs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BuildConfig {
    feed_url: &'static str,
    /// The public key as the build was given it, which is the form the
    /// library takes.
    public_key_text: &'static str,
    public_key: [u8; feed::PUBLIC_KEY_LEN],
}

/// The build's update configuration, or `None` for a build that was
/// given no feed, no key, or one of them in a form that cannot be used:
/// a URL Code++ would not fetch, a key that is not 32 bytes of base64.
/// Such a build has no updater. It does not fall back to anything.
fn build_config(
    feed_url: Option<&'static str>,
    public_key: Option<&'static str>,
) -> Option<BuildConfig> {
    let (feed_url, public_key_text) = (feed_url?, public_key?);
    if !update_feed::is_fetchable(feed_url) {
        return None;
    }
    let public_key_text = public_key_text.trim();
    Some(BuildConfig {
        feed_url,
        public_key_text,
        public_key: feed::decode_public_key(public_key_text)?,
    })
}

/// What the worker tells the UI thread.
#[derive(Debug)]
pub enum UpdaterEvent {
    /// Whether this copy of Code++ can update itself. Sent once, first.
    Availability(bool),
    /// An update was downloaded and verified. The UI quits, and starts
    /// the installer as its last act.
    InstallReady(ReadyInstall),
    /// An update could not be checked for or was refused: why, as a
    /// sentence to show.
    Failed(String),
}

/// A verified installer and what it takes to start it.
///
/// The file is held open, denying writes, for as long as this value
/// lives; dropping it installs nothing and lets the file go.
#[derive(Debug)]
pub struct ReadyInstall {
    installer: VerifiedInstaller,
    kind: InstallKind,
    install_dir: PathBuf,
}

impl ReadyInstall {
    /// The version the installer installs.
    #[must_use]
    pub fn version(&self) -> &str {
        self.installer.version()
    }

    /// Start Windows Installer on the verified file and return its
    /// process id. The caller exits straight after: Windows Installer
    /// cannot replace an executable that is running.
    ///
    /// # Errors
    ///
    /// See [`VerifiedInstaller::launch`].
    pub fn launch(self) -> Result<u32, InstallError> {
        self.installer.launch(self.kind, &self.install_dir)
    }
}

/// Why a downloaded installer was not accepted.
#[derive(Debug)]
enum PrepareError {
    Fetch(FetchError),
    Feed(feed::FeedError),
    Install(InstallError),
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fetch(error) => write!(f, "The update feed could not be read again: {error}."),
            Self::Feed(error) => write!(f, "The update feed could not be read again: {error}."),
            Self::Install(error) => write!(f, "The update was not installed: {error}."),
        }
    }
}

/// Verify the installer at `path` against the signatures `feed_xml`
/// states, and refuse a version that is not newer than `running`.
///
/// The feed is not trusted: it only supplies candidates, and a
/// signature either is the key's signature of this file or is not. The
/// version is the package's own, read after the signature has passed,
/// and not the one the feed states for it.
fn verify_against_feed(
    path: &Path,
    feed_xml: &[u8],
    public_key: &[u8; feed::PUBLIC_KEY_LEN],
    running: &str,
) -> Result<VerifiedInstaller, PrepareError> {
    let items = feed::read_from(feed_xml).map_err(PrepareError::Feed)?;
    let signatures: Vec<_> = items.iter().map(|item| item.signature).collect();
    update_install::verify(path, public_key, &signatures, |version| {
        update::is_newer(version, running)
    })
    .map_err(PrepareError::Install)
}

/// The updater's state file behind a lock, as the place the library
/// keeps its values (in place of the registry) and Code++ the time of
/// its last check.
///
/// Every change is written to the file at once. The library calls from
/// threads of its own, a few times per check.
struct StateStore {
    path: PathBuf,
    state: Mutex<UpdaterState>,
}

impl StateStore {
    /// The state kept at `path`, or an empty one if there is no file
    /// there, or one that cannot be read: the cost of starting over is
    /// one check made a day early and a skipped version offered again.
    fn load(path: PathBuf) -> Self {
        let state = match codepp_platform::open_regular_file(&path) {
            Ok(Some(file)) => UpdaterState::read_from(file).unwrap_or_else(|error| {
                tracing::warn!(error = ?error.to_string(), "updater.xml could not be read; starting it over");
                UpdaterState::default()
            }),
            Ok(None) => {
                tracing::warn!("updater.xml is not a regular file; starting it over");
                UpdaterState::default()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => UpdaterState::default(),
            Err(error) => {
                tracing::warn!(error = ?error.to_string(), "updater.xml could not be opened; starting it over");
                UpdaterState::default()
            }
        };
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    /// Change the state and, if `change` says it changed, save it.
    fn update(&self, change: impl FnOnce(&mut UpdaterState) -> bool) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if change(&mut state) {
            if let Err(error) = state.save(&self.path) {
                tracing::warn!(error = ?error.to_string(), "updater.xml could not be written");
            }
        }
    }

    fn last_check(&self) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_check
    }

    fn set_last_check(&self, now: u64) {
        self.update(|state| {
            state.last_check = Some(now);
            true
        });
    }
}

/// [`StateStore`] as the library's store. A newtype, since the library
/// takes ownership of its store and the worker keeps reading the same
/// state.
struct LibraryStore(Arc<StateStore>);

impl ConfigStore for LibraryStore {
    fn read(&self, name: &str) -> Option<String> {
        let state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.get(name).map(str::to_owned)
    }

    fn write(&self, name: &str, value: &str) {
        self.0.update(|state| {
            if state.get(name) == Some(value) {
                return false;
            }
            // A value outside the file's bounds is not kept, and what
            // was kept under its name goes with it: the library then
            // reads no value, where it would otherwise read an old one.
            state.set(name, value) || state.remove(name)
        });
    }

    fn delete(&self, name: &str) {
        self.0.update(|state| state.remove(name));
    }
}

/// The current time, in seconds since the Unix epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// What makes this copy able to update itself, all of it established on
/// the worker.
struct Installation {
    config: BuildConfig,
    kind: InstallKind,
    install_dir: PathBuf,
    library: PathBuf,
    state_path: PathBuf,
}

/// Establish whether this copy can update itself: see the module's
/// documentation for what that takes. Logs which part is missing, at
/// `debug`, since most copies a developer runs lack all of them.
fn establish() -> Option<Installation> {
    let Some(config) = build_config(BUILD_FEED_URL, BUILD_PUBLIC_KEY) else {
        tracing::debug!("no updater: the build was given no feed and key it can use");
        return None;
    };
    let install_dir = codepp_platform::program_dir()?;
    let Some(kind) = update_install::install_kind(&install_dir) else {
        tracing::debug!("no updater: Windows Installer did not put this copy here");
        return None;
    };
    if kind == InstallKind::PerMachine && !PER_MACHINE_UPDATES {
        tracing::info!("no updater: installations for all users cannot update themselves yet");
        return None;
    }
    let Some(library) = winsparkle::dll_beside_exe() else {
        tracing::debug!(
            "no updater: {} is not beside the executable",
            winsparkle::DLL_NAME
        );
        return None;
    };
    let state_path = codepp_platform::updater_xml_path()?;
    Some(Installation {
        config,
        kind,
        install_dir,
        library,
        state_path,
    })
}

enum Command {
    /// The preference changed, or something else that could make a
    /// check due.
    Reschedule,
    /// The user asked for a check from the menu.
    CheckNow,
    /// The application is quitting.
    Shutdown,
}

/// The UI thread's end of the updater.
pub struct Updater {
    commands: Sender<Command>,
    events: Receiver<UpdaterEvent>,
    /// The Auto-updater preference, which the worker reads whenever it
    /// asks whether a check is due.
    enabled: Arc<AtomicBool>,
    /// Closed by the worker when it has stopped the library.
    stopped: Receiver<()>,
}

impl Updater {
    /// Start the worker. `enabled` is the Auto-updater preference, and
    /// `wake` is called after every event the worker sends.
    ///
    /// Returns at once, having done nothing but start a thread: call it
    /// after the first frame is drawn (DESIGN.md §11.2).
    #[must_use]
    pub fn start(enabled: bool, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (stopped_tx, stopped_rx) = crossbeam_channel::bounded::<()>(0);
        let enabled = Arc::new(AtomicBool::new(enabled));
        let worker = Worker {
            commands: command_rx,
            events: event_tx,
            wake,
            enabled: Arc::clone(&enabled),
        };
        let spawned = std::thread::Builder::new()
            .name("updater".into())
            .spawn(move || {
                worker.run();
                drop(stopped_tx);
            });
        if let Err(error) = spawned {
            // Nothing will ever report availability, so the entry and
            // the preference stay greyed.
            tracing::warn!(error = ?error.to_string(), "the updater's thread could not be started");
        }
        Self {
            commands: command_tx,
            events: event_rx,
            enabled,
            stopped: stopped_rx,
        }
    }

    /// The Auto-updater preference changed.
    pub fn set_enabled(&self, enabled: bool) {
        if self.enabled.swap(enabled, Ordering::Relaxed) != enabled {
            let _ = self.commands.send(Command::Reschedule);
        }
    }

    /// The user asked for a check: run one with the library's window,
    /// whatever the preference says.
    pub fn check_now(&self) {
        let _ = self.commands.send(Command::CheckNow);
    }

    /// The next thing the worker reported, if anything.
    #[must_use]
    pub fn poll(&self) -> Option<UpdaterEvent> {
        self.events.try_recv().ok()
    }

    /// Stop the library, as the application quits, and wait a moment
    /// for it. The library has threads of its own, and stopping them
    /// before the process ends is what its `cleanup` is for. A worker
    /// busy with a request is not waited for beyond [`SHUTDOWN_WAIT`].
    ///
    /// Returns an update the worker verified and the UI had not taken
    /// yet: one that became ready as the quit began, when nothing drains
    /// the worker's events any more. The user asked for it to be
    /// installed, and whether the quit was theirs or the updater's does
    /// not change that.
    #[must_use]
    pub fn shut_down(self) -> Option<ReadyInstall> {
        self.shut_down_within(SHUTDOWN_WAIT)
    }

    /// [`Self::shut_down`], waiting at most `wait` for the worker.
    fn shut_down_within(self, wait: Duration) -> Option<ReadyInstall> {
        if self.commands.send(Command::Shutdown).is_ok() {
            // The channel closing is the signal, which reads as an
            // error. Running out of time is the other one.
            let waited = self.stopped.recv_timeout(wait);
            if waited == Err(crossbeam_channel::RecvTimeoutError::Timeout) {
                tracing::warn!(
                    "the updater's worker was still busy at the quit; what it was doing is \
                     dropped, an update it was verifying included"
                );
            }
        }
        let mut ready = None;
        while let Ok(event) = self.events.try_recv() {
            if let UpdaterEvent::InstallReady(install) = event {
                ready = Some(install);
            }
        }
        ready
    }
}

/// The worker thread's state.
struct Worker {
    commands: Receiver<Command>,
    events: Sender<UpdaterEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
    enabled: Arc<AtomicBool>,
}

impl Worker {
    fn report(&self, event: UpdaterEvent) {
        if self.events.send(event).is_ok() {
            (self.wake)();
        }
    }

    fn run(self) {
        let installation = establish();
        self.report(UpdaterEvent::Availability(installation.is_some()));
        let Some(installation) = installation else {
            return;
        };
        let store = Arc::new(StateStore::load(installation.state_path.clone()));
        let (library_tx, library_rx) = crossbeam_channel::unbounded();
        let mut session = Session {
            installation,
            store,
            library_tx,
            library: None,
            checking_since: None,
        };
        session.check_if_due(self.enabled.load(Ordering::Relaxed));
        // A fixed time, and not a timeout that starts over with every
        // command and event: "every hour" must not become "an hour
        // after the worker last had something to do".
        let mut next_recheck = Instant::now() + RECHECK_EVERY;
        loop {
            let until_recheck = next_recheck.saturating_duration_since(Instant::now());
            crossbeam_channel::select! {
                recv(self.commands) -> command => match command {
                    Ok(Command::Reschedule) => {
                        session.check_if_due(self.enabled.load(Ordering::Relaxed));
                    }
                    Ok(Command::CheckNow) => {
                        if let Err(why) = session.check_with_window() {
                            self.report(UpdaterEvent::Failed(why));
                        }
                    }
                    // The UI's end is gone, or says so.
                    Ok(Command::Shutdown) | Err(_) => break,
                },
                recv(library_rx) -> event => {
                    if let Ok(event) = event {
                        if let Some(report) = session.on_library_event(event) {
                            self.report(report);
                        }
                    }
                },
                default(until_recheck) => {
                    next_recheck = Instant::now() + RECHECK_EVERY;
                    session.check_if_due(self.enabled.load(Ordering::Relaxed));
                }
            }
        }
        if let Some(library) = session.library {
            library.cleanup();
        }
    }
}

/// Whether a scheduled check started at `since` still counts as under
/// way at `now`: one is, until the library answers it or
/// [`CHECK_GIVEN_UP_AFTER`] has passed.
fn check_under_way(since: Option<Instant>, now: Instant) -> bool {
    since.is_some_and(|since| now.saturating_duration_since(since) < CHECK_GIVEN_UP_AFTER)
}

/// What the worker keeps once it knows this copy can update itself.
struct Session {
    installation: Installation,
    store: Arc<StateStore>,
    library_tx: Sender<winsparkle::Event>,
    /// The library, once a check has needed it. A session in which no
    /// check is due and the user asks for none never loads it.
    library: Option<&'static WinSparkle>,
    /// When the scheduled check under way was started, if one is:
    /// another is not started while it is, for up to
    /// [`CHECK_GIVEN_UP_AFTER`].
    checking_since: Option<Instant>,
}

impl Session {
    /// The library, loading and starting it on first use.
    fn library(&mut self) -> Option<&'static WinSparkle> {
        if self.library.is_none() {
            let setup = winsparkle::Setup {
                feed_url: self.installation.config.feed_url,
                public_key: self.installation.config.public_key_text,
                company: "Code++",
                app: "Code++",
                version: codepp_core::VERSION,
            };
            let store = Box::new(LibraryStore(Arc::clone(&self.store)));
            match WinSparkle::start(
                &self.installation.library,
                &setup,
                store,
                self.library_tx.clone(),
            ) {
                Ok(library) => self.library = Some(library),
                Err(error) => {
                    tracing::warn!(error = ?error.to_string(), "the updater library could not be started");
                }
            }
        }
        self.library
    }

    /// Start a scheduled check if one is due and none is under way.
    fn check_if_due(&mut self, enabled: bool) {
        if check_under_way(self.checking_since, Instant::now())
            || !update::check_is_due(enabled, self.store.last_check(), now())
        {
            return;
        }
        if let Some(library) = self.library() {
            self.checking_since = Some(Instant::now());
            library.check_in_background();
        }
    }

    /// Run the check the user asked for.
    fn check_with_window(&mut self) -> Result<(), String> {
        let library = self
            .library()
            .ok_or("The updater could not be started, so no check was made.")?;
        library.check_with_window();
        Ok(())
    }

    /// Take what the library reports, and give what the UI is to hear
    /// of it.
    fn on_library_event(&mut self, event: winsparkle::Event) -> Option<UpdaterEvent> {
        match event {
            // A check that got an answer is a check made. One that
            // failed is not: it is tried again at the next hour, and
            // at the next start.
            winsparkle::Event::FoundUpdate | winsparkle::Event::NoUpdate => {
                self.checking_since = None;
                self.store.set_last_check(now());
                None
            }
            winsparkle::Event::Error => {
                self.checking_since = None;
                None
            }
            winsparkle::Event::InstallerReady(path) => Some(self.prepare(&path)),
            // The quit this asks for comes from Code++ itself, once the
            // installer is verified.
            winsparkle::Event::Cancelled | winsparkle::Event::ShutdownRequested => None,
        }
    }

    /// Verify the installer the library downloaded.
    fn prepare(&self, path: &Path) -> UpdaterEvent {
        let config = &self.installation.config;
        let user_agent = format!("Code++/{}", codepp_core::VERSION);
        let cap = usize::try_from(feed::MAX_FEED_BYTES).unwrap_or(usize::MAX);
        let verified = update_feed::fetch(config.feed_url, &user_agent, cap)
            .map_err(PrepareError::Fetch)
            .and_then(|xml| {
                verify_against_feed(path, &xml, &config.public_key, codepp_core::VERSION)
            });
        match verified {
            Ok(installer) => {
                tracing::info!(version = installer.version(), "update verified");
                UpdaterEvent::InstallReady(ReadyInstall {
                    installer,
                    kind: self.installation.kind,
                    install_dir: self.installation.install_dir.clone(),
                })
            }
            Err(error) => {
                tracing::warn!(error = ?error.to_string(), "update refused");
                UpdaterEvent::Failed(error.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    const KEY_TEXT: &str = "GfY7YUA2b3vvIlE+0mmQzGWrS+73SQL/6/68MxGvhvg=";

    #[test]
    fn a_build_without_a_usable_feed_and_key_has_no_updater() {
        let url = "https://example.org/feed.xml";
        let config = build_config(Some(url), Some(KEY_TEXT)).expect("a usable configuration");
        assert_eq!(config.feed_url, url);
        assert_eq!(config.public_key_text, KEY_TEXT);
        assert_eq!(config.public_key[..4], [0x19, 0xf6, 0x3b, 0x61]);

        assert_eq!(build_config(None, None), None);
        assert_eq!(build_config(Some(url), None), None);
        assert_eq!(build_config(None, Some(KEY_TEXT)), None);
        for bad_url in [
            "",
            "http://example.org/feed.xml",
            "example.org",
            "ftp://example.org/x",
        ] {
            assert_eq!(
                build_config(Some(bad_url), Some(KEY_TEXT)),
                None,
                "{bad_url:?}"
            );
        }
        for bad_key in ["", "not base64", "AAAA", &KEY_TEXT[..40]] {
            // `build_config` takes `'static` text, as the build's is.
            let bad_key: &'static str = Box::leak(bad_key.to_owned().into_boxed_str());
            assert_eq!(build_config(Some(url), Some(bad_key)), None, "{bad_key:?}");
        }
    }

    #[test]
    fn the_store_keeps_the_librarys_values_and_the_last_check_in_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updater.xml");
        let store = Arc::new(StateStore::load(path.clone()));
        assert_eq!(store.last_check(), None);
        assert!(!path.exists(), "reading a state writes nothing");

        let library = LibraryStore(Arc::clone(&store));
        assert_eq!(library.read("SkipThisVersion"), None);
        library.write("SkipThisVersion", "0.2.1");
        library.write("CheckForUpdates", "0");
        store.set_last_check(1_763_000_000);
        assert_eq!(library.read("SkipThisVersion").as_deref(), Some("0.2.1"));
        library.delete("CheckForUpdates");
        library.delete("NeverWritten");

        // Every change is on disk, so another run reads the same.
        let again = StateStore::load(path);
        assert_eq!(again.last_check(), Some(1_763_000_000));
        let again = LibraryStore(Arc::new(again));
        assert_eq!(again.read("SkipThisVersion").as_deref(), Some("0.2.1"));
        assert_eq!(again.read("CheckForUpdates"), None);
    }

    #[test]
    fn a_value_outside_the_files_bounds_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(StateStore::load(dir.path().join("updater.xml")));
        let library = LibraryStore(Arc::clone(&store));
        library.write("UpdateTempDir", &"x".repeat(update::MAX_VALUE_BYTES + 1));
        assert_eq!(library.read("UpdateTempDir"), None);
        library.write("Bad\u{1}Name", "1");
        assert_eq!(library.read("Bad\u{1}Name"), None);
    }

    #[test]
    fn a_state_file_that_cannot_be_read_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updater.xml");
        std::fs::write(&path, b"<updater last-check=\"12\"><value").unwrap();
        let store = StateStore::load(path.clone());
        assert_eq!(store.last_check(), None);
        // And is replaced by a good one at the first change.
        store.set_last_check(34);
        assert_eq!(StateStore::load(path).last_check(), Some(34));
        // A folder where the file should be is not a state either.
        let folder = dir.path().join("folder.xml");
        std::fs::create_dir(&folder).unwrap();
        assert_eq!(StateStore::load(folder).last_check(), None);
    }

    /// Standard base64, for the feeds these tests write.
    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut text = String::new();
        for chunk in bytes.chunks(3) {
            let group = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let sextets = [
                group[0] >> 2,
                (group[0] & 0x03) << 4 | group[1] >> 4,
                (group[1] & 0x0f) << 2 | group[2] >> 6,
                group[2] & 0x3f,
            ];
            for (index, sextet) in sextets.into_iter().enumerate() {
                if index <= chunk.len() {
                    text.push(char::from(ALPHABET[usize::from(sextet)]));
                } else {
                    text.push('=');
                }
            }
        }
        text
    }

    /// A feed with one item per `(version, signature)`.
    fn feed_of(items: &[(&str, &[u8])]) -> Vec<u8> {
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<rss version=\"2.0\" \
             xmlns:sparkle=\"http://www.andymatuschak.org/xml-namespaces/sparkle\"><channel>\n",
        );
        for (version, signature) in items {
            writeln!(
                xml,
                "<item><title>{version}</title><enclosure url=\"https://example.org/x.msi\" \
                 sparkle:version=\"{version}\" sparkle:edSignature=\"{}\" length=\"1\" \
                 type=\"application/octet-stream\"/></item>",
                base64(signature)
            )
            .unwrap();
        }
        xml.push_str("</channel></rss>\n");
        xml.into_bytes()
    }

    fn signed_package(dir: &Path, version: &str) -> (PathBuf, SigningKey, [u8; 64]) {
        let path = dir.join(format!("codepp-{version}.msi"));
        codepp_platform::msi_package::make_test_package(&path, &[("ProductVersion", version)]);
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let signature = key.sign(&std::fs::read(&path).unwrap()).to_bytes();
        (path, key, signature)
    }

    #[test]
    fn an_installer_the_feed_signs_and_that_is_newer_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let (path, key, signature) = signed_package(dir.path(), "0.2.1");
        let public = key.verifying_key().to_bytes();
        // The feed lists an older release first, as feeds do.
        let feed = feed_of(&[("0.2.0", &[3u8; 64]), ("0.2.1", &signature)]);
        let installer = verify_against_feed(&path, &feed, &public, "0.2.0").unwrap();
        assert_eq!(installer.version(), "0.2.1");
        // Held: the file cannot be written while the installer is ready.
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        drop(installer);
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_ok());
    }

    #[test]
    fn the_packages_own_version_decides_and_not_the_feeds() {
        let dir = tempfile::tempdir().unwrap();
        // A properly signed older release, which the feed calls 9.9.9.
        let (path, key, signature) = signed_package(dir.path(), "0.1.9");
        let public = key.verifying_key().to_bytes();
        let feed = feed_of(&[("9.9.9", &signature)]);
        for running in ["0.2.0", "0.1.9", "0.1.9-rc.1"] {
            let refused = verify_against_feed(&path, &feed, &public, running);
            assert!(
                matches!(
                    &refused,
                    Err(PrepareError::Install(InstallError::VersionRefused(v))) if v == "0.1.9"
                ),
                "{running}: {refused:?}"
            );
        }
        assert!(verify_against_feed(&path, &feed, &public, "0.1.8").is_ok());
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_ok());
    }

    #[test]
    fn an_installer_the_feed_does_not_sign_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (path, key, signature) = signed_package(dir.path(), "0.2.1");
        let public = key.verifying_key().to_bytes();

        // The feed changed since the library read it: other signatures.
        let other = feed_of(&[("0.2.1", &[5u8; 64])]);
        assert!(matches!(
            verify_against_feed(&path, &other, &public, "0.2.0"),
            Err(PrepareError::Install(InstallError::BadSignature))
        ));
        // A feed with no item at all, and one that is not a feed.
        assert!(matches!(
            verify_against_feed(&path, &feed_of(&[]), &public, "0.2.0"),
            Err(PrepareError::Install(InstallError::BadSignature))
        ));
        assert!(matches!(
            verify_against_feed(&path, b"<html>moved</html", &public, "0.2.0"),
            Err(PrepareError::Feed(_))
        ));
        // The right feed, and another key built into Code++.
        let feed = feed_of(&[("0.2.1", &signature)]);
        let stranger = SigningKey::from_bytes(&[10u8; 32])
            .verifying_key()
            .to_bytes();
        assert!(matches!(
            verify_against_feed(&path, &feed, &stranger, "0.2.0"),
            Err(PrepareError::Install(InstallError::BadSignature))
        ));
        // The right feed and key, and a file changed since it was signed.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(0);
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            verify_against_feed(&path, &feed, &public, "0.2.0"),
            Err(PrepareError::Install(InstallError::BadSignature))
        ));
    }

    #[test]
    fn a_copy_that_cannot_update_itself_says_so_once_and_stops() {
        // A test binary is no installation: the worker reports that and
        // ends, and the UI's end keeps working without it.
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&woken);
        let updater = Updater::start(
            true,
            Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            }),
        );
        let event = updater
            .events
            .recv_timeout(Duration::from_secs(30))
            .expect("the worker reports");
        assert!(
            matches!(event, UpdaterEvent::Availability(false)),
            "{event:?}"
        );
        // The worker is gone: nothing more arrives, and nothing blocks.
        assert_eq!(
            updater.stopped.recv_timeout(Duration::from_secs(30)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        );
        assert!(updater.poll().is_none());
        assert_eq!(woken.load(Ordering::Relaxed), 1);
        updater.set_enabled(false);
        updater.check_now();
        assert!(updater.shut_down().is_none());
    }

    /// An [`Updater`] whose worker is the test: the ends a worker would
    /// hold are handed back.
    fn updater_by_hand() -> (Updater, Receiver<Command>, Sender<UpdaterEvent>, Sender<()>) {
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (stopped_tx, stopped_rx) = crossbeam_channel::bounded::<()>(0);
        let updater = Updater {
            commands: command_tx,
            events: event_rx,
            enabled: Arc::new(AtomicBool::new(true)),
            stopped: stopped_rx,
        };
        (updater, command_rx, event_tx, stopped_tx)
    }

    #[test]
    fn an_update_verified_as_the_application_quits_is_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let (path, key, signature) = signed_package(dir.path(), "0.2.1");
        let feed = feed_of(&[("0.2.1", &signature)]);
        let installer =
            verify_against_feed(&path, &feed, &key.verifying_key().to_bytes(), "0.2.0").unwrap();
        let ready = ReadyInstall {
            installer,
            kind: InstallKind::PerUser,
            install_dir: dir.path().to_owned(),
        };

        // The worker reports the update, and the quit comes before any
        // drain has taken it: the worker stops when told to.
        let (updater, commands, events, stopped) = updater_by_hand();
        events
            .send(UpdaterEvent::Failed("an earlier refusal".into()))
            .unwrap();
        events.send(UpdaterEvent::InstallReady(ready)).unwrap();
        let worker = std::thread::spawn(move || {
            assert!(matches!(commands.recv(), Ok(Command::Shutdown)));
            drop(stopped);
        });
        let kept = updater
            .shut_down()
            .expect("the verified update is handed back");
        worker.join().unwrap();
        assert_eq!(kept.version(), "0.2.1");
        // Still held, so that it is still the file that was verified.
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_err());
        drop(kept);
        assert!(std::fs::OpenOptions::new().write(true).open(&path).is_ok());
    }

    #[test]
    fn a_worker_that_does_not_stop_is_waited_for_only_so_long() {
        // The worker never reads its commands: busy with a request.
        let (updater, _commands, _events, _stopped) = updater_by_hand();
        let wait = Duration::from_millis(300);
        let before = Instant::now();
        assert!(updater.shut_down_within(wait).is_none());
        let waited = before.elapsed();
        assert!(waited >= wait, "it is waited for: {waited:?}");
        assert!(
            waited < wait * 20,
            "and no longer than a moment: {waited:?}"
        );

        // One that stops at once is not waited for at all.
        let (updater, commands, _events, stopped) = updater_by_hand();
        let worker = std::thread::spawn(move || {
            let _ = commands.recv();
            drop(stopped);
        });
        let before = Instant::now();
        assert!(updater.shut_down_within(Duration::from_secs(30)).is_none());
        assert!(before.elapsed() < Duration::from_secs(10));
        worker.join().unwrap();
    }

    #[test]
    fn a_check_the_library_never_answers_stops_counting_as_under_way() {
        let start = Instant::now();
        assert!(!check_under_way(None, start));
        assert!(check_under_way(Some(start), start));
        let almost = CHECK_GIVEN_UP_AFTER
            .checked_sub(Duration::from_secs(1))
            .expect("the wait is longer than a second");
        assert!(check_under_way(Some(start), start + almost));
        assert!(!check_under_way(Some(start), start + CHECK_GIVEN_UP_AFTER));
        assert!(!check_under_way(
            Some(start),
            start + CHECK_GIVEN_UP_AFTER + RECHECK_EVERY
        ));
        // A clock that reads earlier than the start is not a reason to
        // start a second check.
        assert!(check_under_way(Some(start + Duration::from_secs(5)), start));
    }
}
