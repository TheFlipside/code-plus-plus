//! Headless core for Code++.
//!
//! This crate intentionally has no UI, no Scintilla, and no platform code.
//! It is unit-testable without an OS event loop. See DESIGN.md §2.2.

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        // Phase 0 placeholder so `cargo test -p codepp-core` exercises the test harness.
        assert_eq!(2 + 2, 4);
    }
}
