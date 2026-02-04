use std::collections::HashMap;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::cap_std::fs::Dir;
use indexmap::IndexMap;
use rpm_qa::FileInfo;

use super::{ComponentId, ComponentInfo, ComponentsRepo, FileType};

const REPO_NAME: &str = "rpm";

const RPMDB_PATHS: &[&str] = &["usr/lib/sysimage/rpm", "usr/share/rpm", "var/lib/rpm"];

/// RPM-based components repo implementation.
///
/// Uses the RPM database to determine file ownership and groups files
/// by their SRPM.
pub struct RpmRepo {
    /// Unique component (SRPM) names mapped to (buildtime, stability), indexed by ComponentId.
    components: IndexMap<String, (u64, f64)>,

    /// Mapping from path to list of (ComponentId, FileInfo).
    ///
    /// It's common for directories to be owned by more than one component (i.e.
    /// from _different_ SRPMs). It's much more uncommon for files/symlinks
    /// though we do handle it to ensure reproducible layers.
    path_to_components: HashMap<Utf8PathBuf, Vec<(ComponentId, FileInfo)>>,
}

impl RpmRepo {
    /// Load the RPM database from the given rootfs. The `files` parameter is
    /// used to canonicalize paths from the RPM database.
    ///
    /// Returns `Ok(None)` if no RPM database is detected.
    pub fn load(rootfs: &Dir, files: &super::FileMap, now: u64) -> Result<Option<Self>> {
        if !has_rpmdb(rootfs)? {
            return Ok(None);
        }

        let mut packages =
            rpm_qa::load_from_rootfs_dir(rootfs).context("loading rpmdb from rootfs")?;

        tracing::debug!(packages = packages.len(), "canonicalizing package paths");
        canonicalize_package_paths(rootfs, files, &mut packages)
            .context("canonicalizing package paths")?;

        Self::load_from_packages(packages, now).map(Some)
    }

    pub fn load_from_packages(packages: rpm_qa::Packages, now: u64) -> Result<Self> {
        let mut components: IndexMap<String, (u64, f64)> = IndexMap::new();
        let mut path_to_components: HashMap<Utf8PathBuf, Vec<(ComponentId, FileInfo)>> =
            HashMap::new();

        let package_count = packages.len();
        for pkg in packages.into_values() {
            // Use the source RPM as the component name, falling back to package name
            let component_name: &str = match pkg.sourcerpm.as_deref().map(parse_srpm_name) {
                Some(name) => name,
                None => {
                    tracing::warn!(package = %pkg.name, "missing sourcerpm, using package name");
                    &pkg.name
                }
            };

            let entry = components.entry(component_name.to_string());
            let component_id = ComponentId(entry.index());
            match entry {
                indexmap::map::Entry::Occupied(mut e) => {
                    // Build time across subpackages for a given SRPM can vary.
                    // We want the max() of all of them as the clamp.
                    let (existing_bt, _) = e.get_mut();
                    *existing_bt = (*existing_bt).max(pkg.buildtime);
                }
                indexmap::map::Entry::Vacant(e) => {
                    tracing::trace!(component = %component_name, id = component_id.0, "rpm component created");
                    let stability = calculate_stability(&pkg.changelog_times, pkg.buildtime, now)?;
                    e.insert((pkg.buildtime, stability));
                }
            }

            for (path, file_info) in pkg.files.into_iter() {
                // Accumulate entries for all file types. Skip if this component
                // already owns this path (can happen when multiple subpackages
                // from the same SRPM own the same path).
                let entries = path_to_components.entry(path).or_default();
                if !entries.iter().any(|(id, _)| *id == component_id) {
                    entries.push((component_id, file_info));
                }
            }
        }

        tracing::debug!(
            packages = package_count,
            components = components.len(),
            paths = path_to_components.len(),
            "loaded rpm database"
        );

        Ok(Self {
            components,
            path_to_components,
        })
    }
}

impl ComponentsRepo for RpmRepo {
    fn name(&self) -> &'static str {
        REPO_NAME
    }

    fn default_priority(&self) -> usize {
        10
    }

    fn claims_for_path(&self, path: &Utf8Path, file_type: FileType) -> Vec<ComponentId> {
        // Don't claim RPM database paths - let them fall into chunkah/unclaimed
        if let Ok(rel_path) = path.strip_prefix("/")
            && RPMDB_PATHS.iter().any(|p| rel_path.starts_with(p))
        {
            return Vec::new();
        }

        self.path_to_components
            .get(path)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|(_, fi)| file_info_to_file_type(fi) == Some(file_type))
                    .map(|(id, _)| *id)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn component_info(&self, id: ComponentId) -> ComponentInfo<'_> {
        let (name, (mtime, stability)) = self
            .components
            .get_index(id.0)
            // SAFETY: the ids we're given come from the IndexMap itself when we
            // inserted the element, so it must be valid.
            .expect("invalid ComponentId");
        ComponentInfo {
            name,
            mtime_clamp: *mtime,
            stability: *stability,
        }
    }
}

/// Check if any known RPM database path exists in the rootfs.
//
// This probably should live in rpm-qa-rs instead.
fn has_rpmdb(rootfs: &Dir) -> anyhow::Result<bool> {
    for path in RPMDB_PATHS {
        if rootfs
            .try_exists(path)
            .with_context(|| format!("checking for {path}"))?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Canonicalize all file paths in packages by resolving directory symlinks.
fn canonicalize_package_paths(
    rootfs: &Dir,
    files: &super::FileMap,
    packages: &mut rpm_qa::Packages,
) -> Result<()> {
    let mut cache = HashMap::new();

    for package in packages.values_mut() {
        let old_files = std::mem::take(&mut package.files);
        for (path, info) in old_files {
            let canonical = canonicalize_parent_path(rootfs, files, &path, &mut cache)
                .with_context(|| format!("canonicalizing {}", path))?;
            if canonical != path {
                tracing::trace!(original = %path, canonical = %canonical, "path canonicalized");
            }
            package.files.insert(canonical, info);
        }
    }

    Ok(())
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
fn canonicalize_parent_path(
    rootfs: &Dir,
    files: &super::FileMap,
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
    files: &super::FileMap,
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

/// Parse the SRPM name from a full SRPM filename.
///
/// e.g., "bash-5.2.15-5.fc40.src.rpm" -> "bash"
fn parse_srpm_name(srpm: &str) -> &str {
    // Remove .src.rpm suffix
    let without_suffix = srpm.strip_suffix(".src.rpm").unwrap_or(srpm);

    // Find the last two dashes (version-release)
    // The name is everything before the second-to-last dash
    let parts: Vec<&str> = without_suffix.rsplitn(3, '-').collect();
    if parts.len() >= 3 {
        parts[2]
    } else {
        without_suffix
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
fn calculate_stability(changelog_times: &[u64], buildtime: u64, now: u64) -> Result<f64> {
    use super::{SECS_PER_DAY, STABILITY_LOOKBACK_DAYS, STABILITY_PERIOD_DAYS};

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

fn file_info_to_file_type(fi: &FileInfo) -> Option<FileType> {
    let file_type = (fi.mode as libc::mode_t) & libc::S_IFMT;
    match file_type {
        libc::S_IFDIR => Some(FileType::Directory),
        libc::S_IFREG => Some(FileType::File),
        libc::S_IFLNK => Some(FileType::Symlink),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8Path;
    use cap_std_ext::cap_std::ambient_authority;

    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/fedora.json");

    #[test]
    fn test_parse_srpm_name() {
        // Package names with no dashes in them
        assert_eq!(parse_srpm_name("bash-5.2.15-5.fc40.src.rpm"), "bash");
        assert_eq!(parse_srpm_name("systemd-256.4-1.fc41.src.rpm"), "systemd");
        assert_eq!(parse_srpm_name("python3-3.12.0-1.fc40.src.rpm"), "python3");
        assert_eq!(parse_srpm_name("glibc-2.39-5.fc40.src.rpm"), "glibc");

        // Package names with dashes in them
        assert_eq!(
            parse_srpm_name("python-dateutil-2.8.2-1.fc40.src.rpm"),
            "python-dateutil"
        );
        assert_eq!(
            parse_srpm_name("cairo-dock-plugins-3.4.1-1.fc40.src.rpm"),
            "cairo-dock-plugins"
        );
        assert_eq!(
            parse_srpm_name("xorg-x11-server-1.20.14-1.fc40.src.rpm"),
            "xorg-x11-server"
        );

        // Edge cases with malformed input
        // Only one dash (not enough for N-V-R pattern)
        assert_eq!(parse_srpm_name("name-version"), "name-version");

        // Missing .src.rpm suffix but valid N-V-R pattern
        assert_eq!(parse_srpm_name("bash-5.2.15-5.fc40"), "bash");

        // No dashes at all
        assert_eq!(parse_srpm_name("nodash"), "nodash");
    }

    #[test]
    fn test_claims_for_path() {
        let packages = rpm_qa::load_from_str(FIXTURE).unwrap();
        let repo = RpmRepo::load_from_packages(packages, now_secs()).unwrap();

        // /usr/bin/bash is a file owned by bash
        let claims = repo.claims_for_path(Utf8Path::new("/usr/bin/bash"), FileType::File);
        assert_eq!(claims.len(), 1);
        let info = repo.component_info(claims[0]);
        assert_eq!(info.name, "bash");
        assert_eq!(info.mtime_clamp, 1753299195);

        // /usr/bin/sh is a symlink owned by bash
        let claims = repo.claims_for_path(Utf8Path::new("/usr/bin/sh"), FileType::Symlink);
        assert_eq!(claims.len(), 1);
        let info = repo.component_info(claims[0]);
        assert_eq!(info.name, "bash");

        // /usr/lib64/libc.so.6 is a file owned by glibc
        let claims = repo.claims_for_path(Utf8Path::new("/usr/lib64/libc.so.6"), FileType::File);
        assert_eq!(claims.len(), 1);
        let info = repo.component_info(claims[0]);
        assert_eq!(info.name, "glibc");
        assert_eq!(info.mtime_clamp, 1765791404);

        // Unowned file should not be claimed
        let claims = repo.claims_for_path(Utf8Path::new("/some/unowned/file"), FileType::File);
        assert!(claims.is_empty());

        // RPMDB paths should not be claimed even if technically owned by rpm package
        for rpmdb_path in [
            "/usr/lib/sysimage/rpm/rpmdb.sqlite",
            "/usr/share/rpm/macros",
            "/var/lib/rpm/Packages",
        ] {
            let claims = repo.claims_for_path(Utf8Path::new(rpmdb_path), FileType::File);
            assert!(
                claims.is_empty(),
                "RPMDB path {} should not be claimed",
                rpmdb_path
            );
        }
    }

    #[test]
    fn test_claims_for_path_wrong_type() {
        let packages = rpm_qa::load_from_str(FIXTURE).unwrap();
        let repo = RpmRepo::load_from_packages(packages, now_secs()).unwrap();

        // /usr/bin/bash is a file in RPM, but we query as symlink
        let claims = repo.claims_for_path(Utf8Path::new("/usr/bin/bash"), FileType::Symlink);
        assert!(claims.is_empty());

        // /usr/bin/sh is a symlink in RPM, but we query as file
        let claims = repo.claims_for_path(Utf8Path::new("/usr/bin/sh"), FileType::File);
        assert!(claims.is_empty());
    }

    #[test]
    fn test_shared_directories_claimed_by_multiple_components() {
        let packages = rpm_qa::load_from_str(FIXTURE).unwrap();
        let repo = RpmRepo::load_from_packages(packages, now_secs()).unwrap();

        // /usr/lib/.build-id is a well-known directory shared by many packages
        let claims = repo.claims_for_path(Utf8Path::new("/usr/lib/.build-id"), FileType::Directory);
        assert!(
            claims.len() >= 2,
            "shared dir should be claimed by multiple components"
        );

        // Verify well-known packages from the fixture are among the claims
        let names: std::collections::HashSet<_> = claims
            .iter()
            .map(|id| repo.component_info(*id).name)
            .collect();
        for pkg in ["bash", "glibc", "coreutils"] {
            assert!(names.contains(pkg), "{pkg} should claim /usr/lib/.build-id");
        }
    }

    #[test]
    fn test_load_from_rpmdb_sqlite() {
        use std::process::Command;

        // skip if rpm command is not available
        let rpm_available = Command::new("rpm").arg("--version").output().is_ok();
        if !rpm_available {
            eprintln!("skipping test: rpm command not available");
            return;
        }

        // create a temp rootfs with the rpmdb.sqlite fixture
        let tmp = tempfile::tempdir().unwrap();
        let rpmdb_dir = tmp.path().join("usr/lib/sysimage/rpm");
        std::fs::create_dir_all(&rpmdb_dir).unwrap();
        let fixture_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rpmdb.sqlite");
        std::fs::copy(&fixture_path, rpmdb_dir.join("rpmdb.sqlite")).unwrap();

        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

        let files = crate::scan::Scanner::new(&rootfs).scan().unwrap();
        let repo = RpmRepo::load(&rootfs, &files, now_secs()).unwrap().unwrap();

        // Test that paths we know are in filesystem and setup are claimed
        let claims = repo.claims_for_path(Utf8Path::new("/"), FileType::Directory);
        assert!(!claims.is_empty(), "/ should be claimed");
        assert_eq!(repo.component_info(claims[0]).name, "filesystem");

        let claims = repo.claims_for_path(Utf8Path::new("/etc"), FileType::Directory);
        assert!(!claims.is_empty(), "/etc should be claimed");
        // /etc is owned by filesystem
        assert_eq!(repo.component_info(claims[0]).name, "filesystem");

        let claims = repo.claims_for_path(Utf8Path::new("/etc/passwd"), FileType::File);
        assert!(!claims.is_empty(), "/etc/passwd should be claimed");
        assert_eq!(repo.component_info(claims[0]).name, "setup");
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
