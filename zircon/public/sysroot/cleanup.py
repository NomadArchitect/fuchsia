#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Remove all stray files and subdirectories not present in a JSON list.
The FILE contains a single JSON list of strings, file names relative
to DIR.  If any of these files is missing from DIR, it's an error.
If there is anything else in DIR, it will be removed.

"""

import argparse
import json
import sys
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "dir", type=Path, nargs=1, metavar="DIR", help="Directory to clean up"
    )
    parser.add_argument(
        "expected_files_list",
        type=Path,
        nargs=1,
        metavar="FILE",
        help="File contains a JSON list of expected files",
    )
    parser.add_argument(
        "stamp",
        type=Path,
        nargs=1,
        metavar="STAMP",
        help="File to write at completion",
    )
    args = parser.parse_args()

    [dir, expected_files_list, stamp] = (
        args.dir + args.expected_files_list + args.stamp
    )

    with expected_files_list.open() as f:
        expected_files = set(Path(string) for string in json.load(f))
    expected_dirs = set(file.parent for file in expected_files) - {Path()}

    found_files = set()
    found_dirs = []  # Order matters for the dirs: bottom up.

    # This could be a bit simpler with Path.walk in Python >= 3.12.
    #   for root, dirs, files in dir.walk(top_down=False):
    #     found_files.update(root / name for name in files)
    #     found_dirs.extend(root / name for name in dirs)
    def find_files(this_dir):
        files = set(this_dir.iterdir())
        subdirs = set(path for path in files if path.is_dir())
        files -= subdirs
        found_files.update(file.relative_to(dir) for file in files)
        for subdir in subdirs:
            find_files(subdir)
            # Add children after their recursions have added grandchildren.
            found_dirs.append(subdir.relative_to(dir))
        # Our parent caller will add us next, unless this is the top dir.
        # The top dir (empty path) is never added to the found_dirs list.

    find_files(dir)

    # Ignore any .stamp files generated by GN/Ninja
    found_files = {path for path in found_files if path.suffix != ".stamp"}

    # Fail if expected files are missing.
    if found_files < expected_files:
        for file in sorted(expected_files - found_files):
            print(f"*** Missing file {file!s}", file=sys.stderr)
        return 2

    # Remove any files (not directories) found that were not expected.
    # Order doesn't matter here, so set operations are handy.
    extra_files = found_files - expected_files
    for file in extra_files:
        print(f"NOTE: Removing unexpected file {dir / file!s}", file=sys.stderr)
        (dir / file).unlink()

    # Remove any (now) empty directories: they don't contain any expected
    # files, and any unexpected files are gone now.  Empty subdirectories
    # must be removed bottom up: child directories before their parents.
    extra_dirs = [path for path in found_dirs if path not in expected_dirs]
    for found_dir in extra_dirs:
        found_dir = dir / found_dir
        found_dir.rmdir()
        print(f"NOTE: Removed empty directory {found_dir!s}", file=sys.stderr)

    if extra_files or extra_dirs:
        print(
            f"NOTE: *** Unexpected files were found and removed. This may "
            f"affect build results, so please re-run the build to ensure "
            f"everything is up to date. ***",
            file=sys.stderr,
        )

    stamp.touch(exist_ok=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
