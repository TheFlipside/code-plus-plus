#!/usr/bin/python3

# This script creates a file (test_case.rs) with a mix of complex structs,
# nested enums, and a repetitive loop of functions to hit the 5,000-line mark.
# Stresses LexRust + lifetime / generic / docstring / escaped-string corners.
# Run from anywhere; the output file is gitignored at the repo root.

with open("test_case.rs", "w", encoding="utf-8") as f:
    f.write("//! Stress test for Rust syntax highlighting\n")
    f.write("use std::collections::{HashMap, VecDeque};\n\n")
    for i in range(1, 450): # Generates ~5000 lines
        f.write(f"/// Documentation for module element {i}\n")
        f.write(f"pub struct DataStructure{i}<'a, T> where T: Clone {{\n")
        f.write(f"    pub field_a: &'a str,\n")
        f.write(f"    pub field_b: Vec<T>,\n")
        f.write(f"    pub metadata: HashMap<String, i32>,\n")
        f.write("}\n\n")
        f.write(f"impl<'a, T> DataStructure{i}<'a, T> where T: Clone {{\n")
        f.write("    pub fn new(val: &'a str) -> Self {\n")
        f.write("        println!(\"Escaped string test: \\\"Internal Quote\\\" and hex \\x41\");\n")
        f.write("        Self { field_a: val, field_b: vec![], metadata: HashMap::new() }\n")
        f.write("    }\n")
        f.write("}\n\n")
