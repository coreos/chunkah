use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use ocidir::cap_std::fs::Dir;

use crate::components::FileType;

pub fn get_current_epoch() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time is before UNIX epoch")
        .map(|d| d.as_secs())
}

/// Parse an RFC 3339 timestamp string into a Unix epoch (seconds).
pub fn parse_rfc3339_epoch(s: &str) -> Result<u64> {
    let dt = chrono::DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("parsing RFC 3339 timestamp: {s}"))?;
    u64::try_from(dt.timestamp()).with_context(|| format!("timestamp is negative: {s}"))
}

/// Format a byte count as a human-readable string using binary units.
pub fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * KIB;
    const GIB: f64 = 1024.0 * MIB;

    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1} GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.1} MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else {
        format!("{} B", bytes)
    }
}

/// Returns the peak resident set size (VmHWM) in bytes.
pub fn get_peak_rss() -> Result<u64> {
    let status =
        std::fs::read_to_string("/proc/self/status").context("reading /proc/self/status")?;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmHWM:") {
            // format is "    123456 kB" but whitespace can vary
            let mut parts = value.split_whitespace();
            let kb_str = parts
                .next()
                .with_context(|| format!("malformed VmHWM line: {line}"))?;
            if parts.next() != Some("kB") || parts.next().is_some() {
                anyhow::bail!("unexpected VmHWM format: {}", value.trim());
            }
            let kb: u64 = kb_str
                .parse()
                .with_context(|| format!("parsing VmHWM value: {kb_str}"))?;
            return Ok(kb * 1024);
        }
    }
    anyhow::bail!("VmHWM not found in /proc/self/status");
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

/// Canonicalize the parent directory of a path by resolving symlinks.
///
/// Given `/lib/modules/5.x/vmlinuz`, if `/lib` -> `usr/lib`, returns
/// `/usr/lib/modules/5.x/vmlinuz`. Only symlinks in directory components are
/// resolved, not the final component (the reason is that if the final component
/// is supposed to be a file/directory according to an underlying package database,
/// but it turns out to be symlink, then something is off and we don't want to claim it).
///
/// The path must be absolute.
pub fn canonicalize_parent_path(
    rootfs: &Dir,
    files: &crate::components::FileMap,
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
    files: &crate::components::FileMap,
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

#[cfg(test)]
mod tests {
    use ocidir::cap_std::ambient_authority;

    use super::*;

    #[test]
    fn test_parse_rfc3339_epoch() {
        assert_eq!(
            parse_rfc3339_epoch("2023-11-14T22:13:20Z").unwrap(),
            1700000000
        );
        assert_eq!(parse_rfc3339_epoch("1970-01-01T00:00:00Z").unwrap(), 0);
        assert!(parse_rfc3339_epoch("not-a-date").is_err());
        assert!(parse_rfc3339_epoch("1969-12-31T23:59:59Z").is_err());
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(1048576), "1.0 MiB");
        assert_eq!(format_size(1073741824), "1.0 GiB");
        assert_eq!(format_size(1610612736), "1.5 GiB");
    }

    #[test]
    fn test_get_goarch() {
        assert_eq!(get_goarch(Some("x86_64")), "amd64");
        assert_eq!(get_goarch(Some("aarch64")), "arm64");
        assert_eq!(get_goarch(Some("powerpc64")), "ppc64le");
        assert_eq!(get_goarch(Some("amd64")), "amd64"); // passthrough
        assert_eq!(get_goarch(Some("unknown")), "unknown"); // passthrough
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

        let files = crate::scan::Scanner::new(&rootfs).scan().unwrap();
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
