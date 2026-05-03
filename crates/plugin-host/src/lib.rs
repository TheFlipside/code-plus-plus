//! Notepad++-compatible plugin host. Owns the NPPM/NPPN dispatcher and
//! the lifecycle logic; delegates `LoadLibrary`/`dlopen` mechanics to
//! `codepp-platform`. See DESIGN.md §6.
//!
//! Phase 0: empty. The ABI freezes at the end of Phase 3.
