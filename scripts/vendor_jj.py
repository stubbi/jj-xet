#!/usr/bin/env python3
"""Keeps src/vendor/ in sync with jj-lib.

The vendored files are jj-lib sources, mechanically adapted to live in this
crate, plus our changes. The changes are stored as src/vendor/xet.patch
against the adapted upstream, so moving to a new jj release is: adapt the new
upstream, apply the patch.

    scripts/vendor_jj.py diff         regenerate xet.patch from src/vendor/
    scripts/vendor_jj.py check        fail if xet.patch is out of date
    scripts/vendor_jj.py sync X.Y.Z   pin jj to X.Y.Z and re-vendor

The jj-lib sources are found through `cargo metadata`, so they are exactly the
ones the build uses.
"""

import difflib
import json
import pathlib
import re
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
VENDOR = ROOT / "src" / "vendor"
PATCH = VENDOR / "xet.patch"
FILES = ["local_working_copy.rs", "eol.rs"]


def run(*args, **kwargs):
    result = subprocess.run(args, cwd=ROOT, text=True, **kwargs)
    if result.returncode != 0:
        sys.exit(f"`{' '.join(args)}` failed with exit code {result.returncode}")
    return result


def jj_lib_src():
    metadata = json.loads(
        run("cargo", "metadata", "--format-version", "1", capture_output=True).stdout
    )
    for package in metadata["packages"]:
        if package["name"] == "jj-lib":
            return pathlib.Path(package["manifest_path"]).parent / "src"
    sys.exit("jj-lib not found in cargo metadata")


def adapt(source):
    """Turns a jj-lib source file into the unpatched vendored version."""
    # The upstream test modules depend on jj-lib's test utilities.
    match = re.search(r"^#\[cfg\(test\)\]\nmod tests \{", source, re.MULTILINE)
    if match:
        source = source[: match.start()].rstrip() + "\n"
    source = source.replace("crate::eol::", "crate::vendor::eol::")
    source = re.sub(r"crate::(?!vendor::)", "jj_lib::", source)
    # Documented by vendor/mod.rs instead.
    return source.replace("#![expect(missing_docs)]\n", "")


def write_adapted(out_dir):
    src = jj_lib_src()
    for name in FILES:
        (out_dir / name).write_text(adapt((src / name).read_text()))
    run("rustfmt", "--edition", "2024", *[str(out_dir / name) for name in FILES])


def diff():
    # difflib rather than diff(1), whose output differs between GNU and BSD.
    with tempfile.TemporaryDirectory() as tmp:
        upstream = pathlib.Path(tmp)
        write_adapted(upstream)
        chunks = []
        for name in FILES:
            chunks.extend(
                difflib.unified_diff(
                    (upstream / name).read_text().splitlines(keepends=True),
                    (VENDOR / name).read_text().splitlines(keepends=True),
                    f"a/{name}",
                    f"b/{name}",
                )
            )
        return "".join(chunks)


def sync(version):
    cargo_toml = ROOT / "Cargo.toml"
    text = cargo_toml.read_text()
    text, count = re.subn(
        r'^(jj-(?:cli|lib)) = "=[^"]+"', rf'\1 = "={version}"', text, flags=re.MULTILINE
    )
    if count != 2:
        sys.exit("expected exact jj-cli and jj-lib pins in Cargo.toml")
    cargo_toml.write_text(text)
    run("cargo", "update", "-p", "jj-lib", "-p", "jj-cli")
    write_adapted(VENDOR)
    result = subprocess.run(
        ["patch", "-p1", "--no-backup-if-mismatch", "-d", str(VENDOR), "-i", str(PATCH)],
        capture_output=True,
        text=True,
    )
    print(result.stdout, end="")
    if result.returncode != 0:
        sys.exit(f"xet.patch does not apply to jj {version}; resolve the .rej files by hand")
    run("cargo", "fmt")
    PATCH.write_text(diff())


def main():
    command = sys.argv[1] if len(sys.argv) > 1 else ""
    if command == "diff":
        PATCH.write_text(diff())
    elif command == "check":
        if PATCH.read_text() != diff():
            sys.exit("src/vendor/xet.patch is stale; run scripts/vendor_jj.py diff")
    elif command == "sync" and len(sys.argv) == 3:
        sync(sys.argv[2])
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
