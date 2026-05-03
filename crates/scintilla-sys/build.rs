// Phase 0: no-op. Scintilla and Lexilla are present as git submodules under
// `vendor/` but are not yet compiled. Phase 1 replaces this with a `cc`-driven
// build of the vendored C/C++ source. See DESIGN.md §4.1 and §7.2.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
