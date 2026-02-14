use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use ocidir::cap_std::fs::Dir;

use crate::components::{FileMap, FileType};

pub fn get_current_epoch() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time is before UNIX epoch")
        .map(|d| d.as_secs())
}

/// Returns the OCI/Go architecture string.
///
/// If `arch` is provided, translates it to OCI format.
/// Otherwise, uses the current system architecture.
pub fn get_goarch(arch: Option<&str>) -> &str {
    match arch.unwrap_or(std::env::consts::ARCH) {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "powerpc64" => "ppc64le",
        arch => arch,
    }
}

/// Canonicalize the parent directory of a path by resolving symlinks.
///
/// Given `/lib/modules/5.x/vmlinuz`, if `/lib` -> `usr/lib`, returns
/// `/usr/lib/modules/5.x/vmlinuz`. Only symlinks in directory components are
/// resolved, not the final component (the reason is that if the final component
/// is supposed to be a file/directory according to the rpmdb, but it turns out
/// to be symlink, then something is off and we don't want the RPM to claim it).
///
/// The path must be absolute.
pub fn canonicalize_parent_path(
    rootfs: &Dir,
    files: &FileMap,
    path: &Utf8Path,
    cache: &mut HashMap<Utf8PathBuf, Utf8PathBuf>,
) -> Result<Utf8PathBuf> {
    assert!(path.is_absolute(), "path must be absolute: {}", path);

    if path == Utf8Path::new("/") {
        return Ok(Utf8PathBuf::from("/"));
    }

    // recursively canonicalize the parent
    let parent = path
        .parent()
        .expect("non-root absolute path must have parent");
    let canonical_parent = canonicalize_dir_path(rootfs, files, parent, cache, 0)?;

    let filename = path
        .file_name()
        .expect("non-root absolute path must have filename");
    Ok(canonical_parent.join(filename))
}

/// Maximum depth for symlink resolution to prevent infinite loops.
const MAX_SYMLINK_DEPTH: usize = 40;

/// Recursively canonicalize a directory path by resolving symlinks.
fn canonicalize_dir_path(
    rootfs: &Dir,
    files: &FileMap,
    path: &Utf8Path,
    cache: &mut HashMap<Utf8PathBuf, Utf8PathBuf>,
    depth: usize,
) -> Result<Utf8PathBuf> {
    assert!(path.is_absolute(), "path must be absolute: {}", path);

    if depth > MAX_SYMLINK_DEPTH {
        anyhow::bail!("too many levels of symbolic links: {}", path);
    }

    // check cache first
    if let Some(cached) = cache.get(path) {
        return Ok(cached.clone());
    }

    // base case: root
    if path == Utf8Path::new("/") {
        return Ok(Utf8PathBuf::from("/"));
    }

    // recursively canonicalize the parent
    let parent = path
        .parent()
        .expect("non-root absolute path must have parent");
    let canonical_parent = canonicalize_dir_path(rootfs, files, parent, cache, depth)?;

    let filename = path
        .file_name()
        .expect("non-root absolute path must have filename");
    let current_path = canonical_parent.join(filename);

    let is_symlink = files
        .get(&current_path)
        .map(|fi| fi.file_type == FileType::Symlink)
        // Technically if we fallback here it means it doesn't even exist in the
        // rootfs so it won't even be claimed. But it feels overkill to try to
        // e.g. return an Option and handle that everywhere.
        .unwrap_or(false);

    let canonical = if is_symlink {
        let rel_path = current_path
            .strip_prefix("/")
            .expect("path must be absolute");
        let target = rootfs
            .read_link_contents(rel_path.as_str())
            .with_context(|| format!("reading symlink target for {}", current_path))?;

        let target_utf8 = Utf8Path::from_path(&target)
            .ok_or_else(|| anyhow::anyhow!("non-UTF-8 symlink target for {}", current_path))?;

        if target_utf8.is_absolute() {
            // absolute symlink - recurse to resolve any symlinks in target
            canonicalize_dir_path(rootfs, files, target_utf8, cache, depth + 1)?
        } else {
            // relative symlink - join with parent and normalize
            let resolved = canonical_parent.join(target_utf8);
            let normalized = normalize_path(&resolved)?;
            // recurse to resolve any symlinks in the resolved path
            canonicalize_dir_path(rootfs, files, &normalized, cache, depth + 1)?
        }
    } else {
        current_path
    };

    cache.insert(path.to_owned(), canonical.clone());
    Ok(canonical)
}

/// Normalize a path by resolving `.` and `..` components.
fn normalize_path(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let mut result = Utf8PathBuf::new();
    for component in path.components() {
        use camino::Utf8Component;
        match component {
            Utf8Component::RootDir => result.push("/"),
            Utf8Component::ParentDir => {
                result.pop();
            }
            Utf8Component::Normal(n) => result.push(n),
            Utf8Component::CurDir => {}
            Utf8Component::Prefix(p) => {
                anyhow::bail!("invalid path prefix: {:?}", p);
            }
        }
    }
    Ok(result)
}

/// Calculate stability from changelog timestamps and build time.
///
/// Uses a Poisson model. I used Gemini Pro 3 to analyzing RPM changelogs from
/// Fedora and found that once you filter out high-activity event-driven periods
/// (mass rebuilds, Fedora branching events), package updates over a large
/// enough period generally follow a Poisson distribution.
///
/// The lookback period is limited to STABILITY_LOOKBACK_DAYS (1 year).
/// If there are no changelog entries, the build time is used as a fallback.
pub fn calculate_stability(changelog_times: &[u64], buildtime: u64, now: u64) -> Result<f64> {
    use crate::components::{SECS_PER_DAY, STABILITY_LOOKBACK_DAYS, STABILITY_PERIOD_DAYS};

    let lookback_start = now.saturating_sub(STABILITY_LOOKBACK_DAYS * SECS_PER_DAY);

    // If there are no changelog entries, use the buildtime as a single data point
    let mut relevant_times: Vec<u64> = if changelog_times.is_empty() {
        vec![buildtime]
    } else {
        changelog_times.to_vec()
    };

    // Filter to entries within the lookback window
    relevant_times.retain(|&t| t >= lookback_start);

    if relevant_times.is_empty() {
        // All changelog entries are older than lookback period.
        // No changes in the past year = very stable.
        return Ok(0.99);
    }

    // Find the oldest timestamp in the window
    let oldest = relevant_times.iter().min().copied().unwrap();

    let span_days = (now.saturating_sub(oldest)) as f64 / SECS_PER_DAY as f64;

    if span_days < 1.0 {
        // Very recent package, assume unstable
        return Ok(0.0);
    }

    let num_changes = relevant_times.len() as f64;

    // lambda in our case is changes per day
    let lambda = num_changes / span_days;

    Ok((-lambda * STABILITY_PERIOD_DAYS).exp())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use camino::{Utf8Path, Utf8PathBuf};
    use ocidir::cap_std::{ambient_authority, fs::Dir};

    use super::*;

    #[test]
    fn test_get_goarch() {
        assert_eq!(get_goarch(Some("x86_64")), "amd64");
        assert_eq!(get_goarch(Some("aarch64")), "arm64");
        assert_eq!(get_goarch(Some("powerpc64")), "ppc64le");
        assert_eq!(get_goarch(Some("amd64")), "amd64"); // passthrough
        assert_eq!(get_goarch(Some("unknown")), "unknown"); // passthrough
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn assert_stability_in_range(stability: f64, min: f64, max: f64) {
        assert!(
            stability >= min && stability <= max,
            "stability {stability} not in range [{min}, {max}]"
        );
    }

    #[test]
    fn test_calculate_stability_all_old_entries() {
        use crate::components::SECS_PER_DAY;

        // All entries older than 1 year should return 0.99
        let now = now_secs();
        let old_time = now - (400 * SECS_PER_DAY); // 400 days ago
        let changelog_times = vec![old_time, old_time - SECS_PER_DAY];
        let buildtime = old_time;

        let stability = calculate_stability(&changelog_times, buildtime, now).unwrap();
        assert_eq!(stability, 0.99);
    }

    #[test]
    fn test_calculate_stability_very_recent() {
        // Package built within 1 day should return 0.0
        let now = now_secs();
        let recent_time = now - 3600; // 1 hour ago
        let changelog_times = vec![recent_time];
        let buildtime = recent_time;

        let stability = calculate_stability(&changelog_times, buildtime, now).unwrap();
        assert_eq!(stability, 0.0);
    }

    #[test]
    fn test_calculate_stability_no_changelog_uses_buildtime() {
        use crate::components::SECS_PER_DAY;

        // No changelog entries should use buildtime as fallback
        let now = now_secs();
        let buildtime = now - (30 * SECS_PER_DAY); // 30 days ago
        let changelog_times: Vec<u64> = vec![];

        let stability = calculate_stability(&changelog_times, buildtime, now).unwrap();
        // 1 change over 30 days = lambda of 1/30
        // stability = e^(-lambda * 7) = e^(-7/30) ≈ 0.79
        assert_stability_in_range(stability, 0.75, 0.85);
    }

    #[test]
    fn test_calculate_stability_normal_case() {
        use crate::components::SECS_PER_DAY;

        // Multiple changelog entries within lookback window
        let now = now_secs();
        // 4 changes over 100 days = lambda of 0.04
        // stability = e^(-0.04 * 7) = e^(-0.28) ≈ 0.76
        let changelog_times = vec![
            now - (10 * SECS_PER_DAY),
            now - (30 * SECS_PER_DAY),
            now - (60 * SECS_PER_DAY),
            now - (100 * SECS_PER_DAY),
        ];
        let buildtime = now - (100 * SECS_PER_DAY);

        let stability = calculate_stability(&changelog_times, buildtime, now).unwrap();
        assert_stability_in_range(stability, 0.70, 0.80);
    }

    #[test]
    fn test_calculate_stability_high_frequency() {
        use crate::components::SECS_PER_DAY;

        // Many changes in a short period = low stability
        let now = now_secs();
        // 10 changes over 20 days = lambda of 0.5
        // stability = e^(-0.5 * 7) = e^(-3.5) ≈ 0.03
        let changelog_times: Vec<u64> = (0..10)
            .map(|i| now - ((2 + i * 2) * SECS_PER_DAY))
            .collect();
        let buildtime = now - (20 * SECS_PER_DAY);

        let stability = calculate_stability(&changelog_times, buildtime, now).unwrap();
        assert_stability_in_range(stability, 0.0, 0.10);
    }

    fn build_filemap(rootfs: &Dir) -> crate::components::FileMap {
        crate::scan::Scanner::new(rootfs).scan().unwrap()
    }

    #[test]
    fn test_normalize_path() {
        let cases = [
            ("/", "/"),
            ("/a/..", "/"),
            ("/a/b/../c", "/a/c"),
            ("/a/./b/c", "/a/b/c"),
            ("/a/b/c/..", "/a/b"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_path(Utf8Path::new(input)).unwrap(),
                Utf8PathBuf::from(expected),
                "normalize_path({input})"
            );
        }
    }

    #[test]
    fn test_canonicalize_path() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();
        rootfs.create_dir_all("usr/lib/modules").unwrap();
        rootfs.symlink("usr/lib", "lib").unwrap();
        rootfs.create_dir_all("usr/bar").unwrap();
        rootfs.symlink(".././../bar", "foo").unwrap();
        rootfs.symlink("usr/bar", "bar").unwrap();

        let files = build_filemap(&rootfs);
        let mut cache = HashMap::new();

        // Test canonicalize_dir_path cases
        let dir_cases = [
            // No symlinks
            ("/usr/lib/modules", "/usr/lib/modules"),
            // Single symlink: /lib -> usr/lib
            ("/lib", "/usr/lib"),
            ("/lib/modules", "/usr/lib/modules"),
            // Symlink chain: /foo -> bar -> usr/bar
            ("/foo", "/usr/bar"),
            // Nonexistent path returns as-is
            ("/nonexistent/path", "/nonexistent/path"),
        ];
        for (input, expected) in dir_cases {
            let result =
                canonicalize_dir_path(&rootfs, &files, Utf8Path::new(input), &mut cache, 0);
            assert_eq!(
                result.unwrap(),
                Utf8PathBuf::from(expected),
                "canonicalize_dir_path({input})"
            );
        }

        // Test canonicalize_parent_path (resolves parent symlinks, keeps filename)
        let parent_cases = [
            ("/lib/modules/vmlinuz", "/usr/lib/modules/vmlinuz"),
            ("/foo/baz", "/usr/bar/baz"),
        ];
        for (input, expected) in parent_cases {
            let result =
                canonicalize_parent_path(&rootfs, &files, Utf8Path::new(input), &mut cache);
            assert_eq!(
                result.unwrap(),
                Utf8PathBuf::from(expected),
                "canonicalize_parent_path({input})"
            );
        }
    }
}
