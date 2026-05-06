//! Code++ preinstalled plugin: MIME-style selection encode/decode.
//!
//! Clean-room MIT reimplementation of the canonical Notepad++ `mimeTools`
//! plugin. Operates on the active editor's selection: replaces it with
//! the encoded or decoded form. Six menu items in this scaffold (Base64
//! Encode/Decode, URL Encode/Decode, Quoted-Printable Encode/Decode);
//! the bodies are filled in subsequent Phase 4 m7 commits.
//!
//! See DESIGN.md §6.6 for the rationale (default-set parity for users
//! coming from N++, plus a stronger ABI smoke test than `example-hello`
//! alone).

#[cfg(target_os = "windows")]
mod imp;
