//! Headless core for Code++.
//!
//! This crate intentionally has no UI, no Scintilla, and no platform
//! code. It is unit-testable without an OS event loop. See DESIGN.md
//! §2.2 and §5.1–§5.2.

pub mod encoding;
pub mod eol;
pub mod fif;
pub mod file;
pub mod find_history;
pub mod lang;
pub mod npp_session;
pub mod session;
pub mod styles;

pub use encoding::{Encoding, EncodingError};
pub use eol::Eol;
pub use fif::{
    FifMatch, FifQuery, FifQueryError, FifQueryOpts, FifWalkOpts, FifWalkOptsError,
    FileSearchOutcome,
};
pub use file::{
    LoadError, LoadErrorKind, LoadResult, LoadedFile, Loader, LoaderShutdown, RequestId,
};
pub use find_history::{FindHistory, FindHistoryError};
pub use lang::LangType;
pub use session::{Session, SessionError, Tab, WindowGeometry};
pub use styles::{format_rgb_hex, parse_rgb_hex, StyleEntry, Styles, StylesError, Transparency};
