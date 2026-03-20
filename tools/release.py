#!/usr/bin/env python3
"""Release tools for chunkah."""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

NAME = "chunkah"


def main():
    parser = argparse.ArgumentParser(description="Release tools for chunkah")
    subparsers = parser.add_subparsers(required=True)

    bump_parser = subparsers.add_parser(
        "bump", help="Bump version in all project files")
    bump_parser.add_argument(
        "version", help="New version (e.g., 0.3.0)")
    bump_parser.add_argument(
        "--pr", action="store_true",
        help="Push branch and open a pull request via gh")
    bump_parser.set_defaults(
        func=lambda args: bump_version(args.version, args.pr))

    cut_parser = subparsers.add_parser(
        "cut", help="Cut a release")
    cut_parser.add_argument(
        "version", help="Version to release (e.g., 0.3.0)")
    cut_parser.add_argument(
        "--no-push", action="store_true",
        help="Prepare release without pushing to remote")
    cut_parser.set_defaults(
        func=lambda args: cut_release(args.version, args.no_push))

    args = parser.parse_args()
    args.func(args)


def bump_version(new_version: str, open_pr: bool = False):
    """Bump version across all project files."""
    if new_version.startswith("v"):
        die(f"Version should not start with 'v' (got '{new_version}'), "
            f"try: {new_version[1:]}")
    if not re.fullmatch(r'\d+\.\d+\.\d+', new_version):
        die(f"Invalid version format: '{new_version}' "
            "(expected MAJOR.MINOR.PATCH, e.g. 0.3.0)")

    if is_worktree_dirty():
        die("Worktree is dirty, commit or stash changes first")

    old_version = get_current_version()
    if old_version == new_version:
        die(f"Version is already {new_version}")

    step(f"Bumping version: {old_version} -> {new_version}")

    update_file("Cargo.toml",
                f'version = "{old_version}"',
                f'version = "{new_version}"')

    step("Updating Cargo.lock...")
    run("cargo", "update", "chunkah")

    update_file("packaging/chunkah.spec",
                f"Version:        {old_version}",
                f"Version:        {new_version}")

    update_file("README.md",
                f"download/v{old_version}/",
                f"download/v{new_version}/")

    step("Updating spec License tag...")
    update_spec_license("packaging/chunkah.spec")

    step("Running version check...")
    run("just", "versioncheck")

    step("Committing changes...")
    run("git", "commit", "-am",
        f"Cargo.toml: bump version to v{new_version}")

    if open_pr:
        branch = f"bump-v{new_version}"
        current_branch = run_output(
            "git", "branch", "--show-current").strip()
        if current_branch != branch:
            step(f"Creating branch {branch}...")
            run("git", "checkout", "-b", branch)
        remote_exists = run_output(
            "git", "ls-remote", "--heads", "origin", branch).strip() != ""
        step("Pushing branch...")
        if remote_exists:
            run("git", "push", "-u", "-f", "origin", branch)
        else:
            run("git", "push", "-u", "origin", branch)
            step("Opening pull request...")
            run("gh", "pr", "create",
                "--title", f"Cargo.toml: bump version to v{new_version}",
                "--body", "")

    print()
    print(f"Version bumped: {old_version} -> {new_version}")


def update_file(path: str, old: str, new: str):
    """Replace old with new in a file, ensuring exactly one match."""
    content = Path(path).read_text()
    count = content.count(old)
    if count == 0:
        die(f"Could not find '{old}' in {path}")
    if count > 1:
        die(f"Found {count} matches for '{old}' in {path} (expected 1)")
    content = content.replace(old, new)
    Path(path).write_text(content)
    step(f"Updated {path}")


def update_spec_license(path: str):
    """Update the License tag and its comment block in the spec file."""
    summary_lines, license_tag = compute_license_tag()
    comment_block = "\n".join(summary_lines)
    new_block = f"{comment_block}\nLicense:        {license_tag}"

    content = Path(path).read_text()
    # Match the comment block + License line between Summary and URL lines.
    # The block is: one or more comment lines followed by the License: line.
    pattern = r'(?:^#[^\n]*\n)+^License:\s+.*$'
    match = re.search(pattern, content, re.MULTILINE)
    if not match:
        die(f"Could not find License block in {path}")
    content = content[:match.start()] + new_block + content[match.end():]
    Path(path).write_text(content)
    step(f"Updated {path}")


def compute_license_tag() -> tuple[list[str], str]:
    """Compute the spec License tag from cargo dependency licenses.

    Returns a tuple of (summary_lines, license_tag) where summary_lines
    are the '# expr' comment lines and license_tag is the SPDX expression
    for the License: field.
    """
    output = run_output(
        "cargo", "tree",
        "--edges=no-build,no-dev,no-proc-macro",
        "--no-dedupe", "--target=all",
        "--prefix=none", "--format={l}")

    # Normalize: replace '/' with ' OR ' (informal shorthand used by some crates)
    lines = set()
    for line in output.strip().splitlines():
        line = line.replace(" / ", "/").replace("/", " OR ")
        lines.add(line)

    # Build the summary comment lines (sorted, with '# ' prefix)
    summary_lines = sorted(f"# {line}" for line in lines)

    # Compute the License tag by ANDing all unique expressions.
    # Expressions containing OR are wrapped in parens to preserve
    # precedence. Standalone and AND-only expressions are kept as-is.
    standalone = sorted(e for e in lines if " OR " not in e)
    choices = sorted(f"({e})" for e in lines if " OR " in e)
    license_tag = " AND ".join(standalone + choices)

    return summary_lines, license_tag


def cut_release(version: str, no_push: bool):
    """Cut a release for chunkah."""
    tag = f"v{version}"
    source_tarball = f"{NAME}-{version}.tar.gz"
    vendor_tarball = f"{NAME}-{version}-vendor.tar.gz"
    notes_file = Path(f".release-notes-{version}.md")

    try:
        if tag_exists(tag):
            die(f"Tag {tag} already exists")

        if is_worktree_dirty():
            die("Worktree is dirty, commit or stash changes first")

        # do this first to avoid building implicitly bumping the lockfile
        step("Running version check...")
        run("just", "versioncheck")

        step("Running checks...")
        run("just", "checkall")

        step("Verifying version matches Cargo.toml...")
        verify_version(version)

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

        step("Generating source and vendor tarballs...")
        generate_archives(source_tarball, vendor_tarball)

        step("Verifying offline build...")
        verify_offline_build(version, source_tarball, vendor_tarball)

        if no_push:
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


def get_current_version() -> str:
    """Get the current version from Cargo.toml via cargo metadata."""
    metadata = json.loads(run_output(
        "cargo", "metadata", "--no-deps", "--format-version=1"))
    return metadata["packages"][0]["version"]


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
    actual = get_current_version()
    if actual != expected:
        die(f"Version mismatch: Cargo.toml has {actual}, but releasing {expected}")


def tag_exists(tag: str) -> bool:
    """Check if a git tag exists."""
    return run_output("git", "tag", "-l", tag).strip() != ""


def is_worktree_dirty() -> bool:
    """Check if the git worktree has uncommitted changes."""
    return run_output("git", "status", "--porcelain").strip() != ""


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


def generate_archives(source_tarball: str, vendor_tarball: str):
    """Generate source and vendor tarballs using create-archives.sh."""
    run("tools/create-archives.sh", source_tarball, vendor_tarball)


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
