//! The auto-updater's state file, `updater.xml`, behind a lock
//! (DESIGN.md §11.3).
//!
//! Shared by the two platforms whose copies update themselves: the
//! Windows worker (`crate::updater`) and the Cocoa backend both keep
//! the time of Code++'s own last check here, so that deciding whether a
//! check is due loads no library.

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use codepp_core::update::UpdaterState;

/// The updater's state file behind a lock: the time of Code++'s last
/// check and, on Windows, the values the library keeps there in place
/// of the registry.
///
/// Every change is written to the file at once. On Windows the library
/// calls from threads of its own, a few times per check.
pub struct StateStore {
    path: PathBuf,
    pub(crate) state: Mutex<UpdaterState>,
}

impl StateStore {
    /// The state kept at `path`, or an empty one if there is no file
    /// there, or one that cannot be read: the cost of starting over is
    /// one check made a day early and a skipped version offered again.
    #[must_use]
    pub fn load(path: PathBuf) -> Self {
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
    pub(crate) fn update(&self, change: impl FnOnce(&mut UpdaterState) -> bool) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if change(&mut state) {
            if let Err(error) = state.save(&self.path) {
                tracing::warn!(error = ?error.to_string(), "updater.xml could not be written");
            }
        }
    }

    /// When Code++ last made a check that got an answer, in seconds
    /// since the Unix epoch.
    #[must_use]
    pub fn last_check(&self) -> Option<u64> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_check
    }

    /// Record a check that got an answer at `now`, and save.
    pub fn set_last_check(&self, now: u64) {
        self.update(|state| {
            state.last_check = Some(now);
            true
        });
    }
}

/// The current time, in seconds since the Unix epoch.
#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
