#!/usr/bin/python3

# This script generates a file (test_case.cpp) full of templates, preprocessor macros,
# and multi-line comments. Used to stress LexCPP highlighting and the line-number
# margin's visible-window populate. Run from anywhere; the file is written into
# the current working directory and is gitignored at the repo root.

with open("test_case.cpp", "w", encoding="utf-8") as f:
    f.write("#include <iostream>\n#include <vector>\n#include <string>\n\n")
    f.write("#define MAX_BUFFER_SIZE 1024\n\n")
    for i in range(1, 400):
        f.write(f"template <typename T{i}>\n")
        f.write(f"class TestClass{i} {{\n")
        f.write("private:\n    T" + str(i) + " data;\n")
        f.write("public:\n")
        f.write(f"    /* Multi-line comment for \n       class number {i} */\n")
        f.write(f"    void process{i}(std::vector<std::string>& vec) {{\n")
        f.write(f"        std::cout << \"Processing {i}...\" << std::endl;\n")
        f.write("        for(auto& s : vec) { if(s == \"test\") return; }\n")
        f.write("    }\n};\n\n")
