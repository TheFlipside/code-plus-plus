//! Headless core for Code++.
//!
//! This crate intentionally has no UI, no Scintilla, and no platform
//! code. It is unit-testable without an OS event loop. See DESIGN.md
//! §2.2 and §5.1–§5.2.

pub mod encoding;
pub mod eol;
pub mod file;
pub mod lang;
pub mod session;

pub use encoding::{Encoding, EncodingError};
pub use eol::Eol;
pub use file::{
    LoadError, LoadErrorKind, LoadResult, LoadedFile, Loader, LoaderShutdown, RequestId,
};
pub use lang::LangType;
pub use session::{Session, SessionError, Tab};
