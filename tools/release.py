#!/usr/bin/env python3
"""Cut a release for chunkah."""

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

NAME = "chunkah"


def main():
    parser = argparse.ArgumentParser(description="Cut a release for chunkah")
    parser.add_argument("version", help="Version to release (e.g., 0.1.0)")
    parser.add_argument("--no-push", action="store_true",
                        help="Prepare release without pushing to remote")
    args = parser.parse_args()

    tag = f"v{args.version}"
    source_tarball = f"{NAME}-{args.version}.tar.gz"
    vendor_tarball = f"{NAME}-{args.version}-vendor.tar.gz"
    notes_file = Path(f".release-notes-{args.version}.md")

    try:
        if tag_exists(tag):
            die(f"Tag {tag} already exists")

        step("Verifying Cargo.lock is in sync...")
        run("just", "lockcheck")

        step("Running checks...")
        run("just", "checkall")

        step("Verifying version matches Cargo.toml...")
        verify_version(args.version)

        # Check for saved notes from a previous failed run
        if notes_file.exists():
            print(f"Found saved release notes from previous run: {notes_file}")
            notes = notes_file.read_text()
        else:
            step("Fetching release notes from GitHub...")
            notes = fetch_release_notes(tag)

        step("Opening editor for release notes...")
        edited_notes = edit_notes(notes)
        if not edited_notes.strip():
            die("Release notes are empty, aborting")

        # Save notes immediately after editing
        notes_file.write_text(edited_notes)

        step(f"Creating signed tag {tag}...")
        create_signed_tag(tag, edited_notes)

        step("Generating source tarball...")
        generate_source_tarball(tag, args.version, source_tarball)

        step("Generating vendor tarball...")
        generate_vendor_tarball(vendor_tarball)

        step("Verifying offline build...")
        verify_offline_build(args.version, source_tarball, vendor_tarball)

        if args.no_push:
            print()
            print(f"Release {tag} prepared successfully.")
            print(f"Tarballs: {source_tarball}, {vendor_tarball}")
            print()
            print("To complete the release, run:")
            print(f"  git push origin {tag}")
            print(f"  gh release create {tag} --notes-from-tag --verify-tag "
                  f"{source_tarball} {vendor_tarball} Containerfile.splitter")
            print(f"  rm {source_tarball} {vendor_tarball}")
        else:
            step("Pushing tag...")
            run("git", "push", "origin", tag)

            step("Creating GitHub release...")
            run("gh", "release", "create", tag, "--notes-from-tag",
                "--verify-tag", source_tarball, vendor_tarball,
                "Containerfile.splitter")

            step("Cleaning up tarballs...")
            os.remove(source_tarball)
            os.remove(vendor_tarball)

            print()
            print(f"Release {tag} published successfully!")

        # Clean up notes file on success
        if notes_file.exists():
            notes_file.unlink()

    except subprocess.CalledProcessError as e:
        if notes_file.exists():
            print(f"Release notes saved to: {notes_file}", file=sys.stderr)
        die(f"Command failed: {e.cmd}")
    except Exception as e:
        if notes_file.exists():
            print(f"Release notes saved to: {notes_file}", file=sys.stderr)
        die(str(e))


def step(msg: str):
    print(f"==> {msg}")


def die(msg: str):
    print(f"Error: {msg}", file=sys.stderr)
    sys.exit(1)


def run(*args: str):
    """Run a command."""
    subprocess.check_call(args)


def run_output(*args: str) -> str:
    """Run a command and return its stdout."""
    return subprocess.check_output(args, text=True)


def verify_version(expected: str):
    """Verify Cargo.toml version matches expected."""
    metadata = json.loads(run_output(
        "cargo", "metadata", "--no-deps", "--format-version=1"))
    actual = metadata["packages"][0]["version"]
    if actual != expected:
        die(f"Version mismatch: Cargo.toml has {actual}, but releasing {expected}")


def tag_exists(tag: str) -> bool:
    """Check if a git tag exists."""
    return run_output("git", "tag", "-l", tag).strip() != ""


def fetch_release_notes(tag: str) -> str:
    """Fetch auto-generated release notes from GitHub."""
    return run_output("gh", "api", "--method", "POST",
                      "repos/:owner/:repo/releases/generate-notes",
                      "-f", f"tag_name={tag}", "--jq", ".body")


def edit_notes(initial: str) -> str:
    """Open editor for user to edit release notes."""
    with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False) as f:
        f.write(initial)
        tmp_path = f.name

    try:
        editor = os.environ.get("EDITOR") or os.environ.get("VISUAL") or "vi"
        subprocess.check_call([editor, tmp_path])
        return Path(tmp_path).read_text()
    finally:
        os.unlink(tmp_path)


def create_signed_tag(tag: str, message: str):
    """Create a signed annotated git tag."""
    with tempfile.NamedTemporaryFile(mode="w", suffix=".md") as f:
        f.write(message)
        f.flush()
        # Use a non-# commentchar so that markdown headers are preserved
        run("git", "-c", "core.commentchar=;", "tag", "-s", "-a", tag,
            "-F", f.name)


def generate_source_tarball(tag: str, version: str, output: str):
    """Generate source tarball using git archive."""
    prefix = f"{NAME}-{version}/"
    run("git", "archive", "--format=tar.gz", f"--prefix={prefix}",
        "-o", output, tag)


def generate_vendor_tarball(output: str):
    """Generate vendored dependencies tarball."""
    run("cargo", "vendor-filterer",
        "--platform", "*-unknown-linux-*",
        "--format=tar.gz",
        "--prefix=vendor",
        output)


def verify_offline_build(version: str, source: str, vendor: str):
    """Verify that the tarballs can build and test offline."""
    with tempfile.TemporaryDirectory() as tmpdir:
        tmpdir = Path(tmpdir)
        project_dir = tmpdir / f"{NAME}-{version}"

        # Extract tarballs
        run("tar", "-xzf", source, "-C", str(tmpdir))
        run("tar", "-xzf", vendor, "-C", str(project_dir))

        # Write cargo config
        cargo_dir = project_dir / ".cargo"
        cargo_dir.mkdir(parents=True, exist_ok=True)
        (cargo_dir / "config.toml").write_text("""\
[source.crates-io]
replace-with = "vendored-sources"
[source.vendored-sources]
directory = "vendor"
""")

        # Build and test
        manifest = project_dir / "Cargo.toml"
        run("cargo", "build", "--release", "--offline",
            "--manifest-path", str(manifest))
        run("cargo", "test", "--release", "--offline",
            "--manifest-path", str(manifest))


if __name__ == "__main__":
    main()
