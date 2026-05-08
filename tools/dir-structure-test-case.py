#!/usr/bin/python3

# For search testing, create a mix of shallow and deep directories,
# plus "needle in a haystack" scenarios. Used to stress find-in-files
# at scale. The placement is intentionally non-deterministic — every
# run produces a different layout so the FIF traversal exercises a
# fresh distribution rather than one canned ordering. The
# `SECRET_KEY = 'FOUND_ME_EXTRACT_THIS'` string is a clearly-fake
# placeholder tagged for the search assertion; nothing sensitive.
#
# Run this script from the directory you want the fixtures to live
# in; the output `test_files/` tree is gitignored at the repo root
# so a careless `git add .` after a run from the repo root won't
# track the 1000 generated files.

import os
import random


def generate_test_suite(base_path="test_files", count=1000):
    words = ["apple", "banana", "cherry", "delta", "echo", "foxtrot", "golf", "hotel"]
    os.makedirs(base_path, exist_ok=True)

    # Hidden "Needle" for your search test
    needle_index = random.randint(0, count - 1)

    for i in range(count):
        # Create varying directory depths
        depth = random.choice(["", "logs/", "src/components/", "docs/archive/old/"])
        path = os.path.join(base_path, depth)
        os.makedirs(path, exist_ok=True)

        filename = f"file_{i}.txt"
        with open(os.path.join(path, filename), "w", encoding="utf-8") as f:
            if i == needle_index:
                f.write("SECRET_KEY = 'FOUND_ME_EXTRACT_THIS'\n")
            else:
                # Random gibberish/content
                content = " ".join(random.choices(words, k=20))
                f.write(f"Random content {i}: {content}\n")

    print(f"Generated {count} files in '{base_path}'")
    print(f"The 'needle' is in file_{needle_index}.txt")


generate_test_suite()