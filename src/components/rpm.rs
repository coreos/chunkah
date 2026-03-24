use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::cap_std::fs::Dir;
use indexmap::IndexMap;
use openssl::hash::{Hasher, MessageDigest};
use rpm_qa::FileInfo;

use crate::utils::{calculate_stability, canonicalize_parent_path};

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

    /// SHA-256 digest index for orphaned RPM files (path in rpmdb but not on
    /// rootfs). Maps hex digest to ComponentId. See build_orphan_digest_index()
    /// for more info.
    orphan_digest_index: HashMap<String, ComponentId>,

    /// File sizes present in the orphan digest index, for fast filtering.
    orphan_sizes: HashSet<u64>,
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

        let mut repo = Self::load_from_packages(packages, now)?;
        build_orphan_digest_index(&mut repo, files);
        Ok(Some(repo))
    }

    pub fn load_from_packages(packages: rpm_qa::Packages, now: u64) -> Result<Self> {
        let mut components: IndexMap<String, (u64, f64)> = IndexMap::new();
        let mut path_to_components: HashMap<Utf8PathBuf, Vec<(ComponentId, FileInfo)>> =
            HashMap::new();

        let package_count = packages.len();
        let mut non_sha256_count: usize = 0;
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
            let stability = calculate_stability(&pkg.changelog_times, pkg.buildtime, now);
            let component_id = ComponentId(entry.index());
            match entry {
                indexmap::map::Entry::Occupied(mut e) => {
                    // Build time across subpackages for a given SRPM can vary.
                    // We want the max() of all of them as the clamp.
                    let (existing_bt, existing_stability) = e.get_mut();
                    *existing_bt = (*existing_bt).max(pkg.buildtime);
                    if stability != *existing_stability && !pkg.changelog_times.is_empty() {
                        // Stability was derived from changelogs only and yet
                        // they're different? This likely means that the RPMs
                        // coming from different versions of the same SRPM are
                        // intermixed in the rootfs. This yields suboptimal
                        // packing and likely indicates a compose bug. Warn.
                        tracing::warn!(package = %pkg.name, "package has different changelog than sibling RPM");
                    }
                    // for determinism, we want the min() of all stabilities if they differ.
                    *existing_stability = (*existing_stability).min(stability);
                    tracing::trace!(component = %component_name, buildtime = %existing_bt, stability = %existing_stability, "multiple rpm components from same srpm");
                }
                indexmap::map::Entry::Vacant(e) => {
                    tracing::trace!(component = %component_name, id = component_id.0, "rpm component created");
                    e.insert((pkg.buildtime, stability));
                }
            }

            // We only understand SHA-256 digests for content matching during
            // weak path claiming. Clear digest fields from packages using other
            // algorithms so that build_orphan_digest_index naturally skips
            // them.
            let is_sha256 = match pkg.digest_algo {
                Some(rpm_qa::DigestAlgorithm::Sha256) => true,
                Some(_) => {
                    non_sha256_count += 1;
                    false
                }
                None => false,
            };

            for (path, mut file_info) in pkg.files.into_iter() {
                if !is_sha256 {
                    file_info.digest = None;
                }
                // Accumulate entries for all file types. Skip if this component
                // already owns this path (can happen when multiple subpackages
                // from the same SRPM own the same path).
                let entries = path_to_components.entry(path).or_default();
                if !entries.iter().any(|(id, _)| *id == component_id) {
                    entries.push((component_id, file_info));
                }
            }
        }

        if non_sha256_count > 0 {
            tracing::warn!(
                non_sha256_count,
                "packages with non-SHA-256 digest algorithm, move detection disabled for these"
            );
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
            orphan_digest_index: HashMap::new(),
            orphan_sizes: HashSet::new(),
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

    fn strong_claims_for_path(
        &self,
        path: &Utf8Path,
        file_info: &super::FileInfo,
    ) -> Vec<ComponentId> {
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
                    .filter(|(_, fi)| file_info_to_file_type(fi) == Some(file_info.file_type))
                    .map(|(id, _)| *id)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn weak_claims_for_path(
        &self,
        rootfs: &Dir,
        path: &Utf8Path,
        file_info: &super::FileInfo,
    ) -> Result<Vec<ComponentId>> {
        if self.orphan_digest_index.is_empty() {
            return Ok(vec![]);
        }

        // only regular files can be matched by digest
        if file_info.file_type != super::FileType::File {
            return Ok(vec![]);
        }

        // fast filter: skip files whose size doesn't match any orphan
        if !self.orphan_sizes.contains(&file_info.size) {
            return Ok(vec![]);
        }

        let digest = compute_sha256(rootfs, path)
            .with_context(|| format!("hashing {path} for weak claim"))?;
        if let Some(id) = self.orphan_digest_index.get(&digest) {
            // SAFETY: the id we put in orphan_digest_index comes from self.components itself!
            tracing::trace!(path = %path, component = self.components.get_index(id.0).unwrap().0, "reclaimed by digest");
            return Ok(vec![*id]);
        }

        Ok(vec![])
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

/// Build an index mapping SHA-256 digests of orphaned RPM files to their
/// ComponentIds. An "orphaned" file is one recorded in the RPM database but
/// not present at its declared path on the rootfs (e.g. moved by compose
/// tooling). Ambiguous digests (mapping to multiple different ComponentIds)
/// are excluded.
fn build_orphan_digest_index(repo: &mut RpmRepo, files: &super::FileMap) {
    // first pass: collect digest -> Vec<ComponentId> for orphaned files
    let mut digest_to_ids: HashMap<String, Vec<ComponentId>> = HashMap::new();
    let mut digest_to_size: HashMap<String, u64> = HashMap::new();

    for (path, entries) in &repo.path_to_components {
        // skip paths that exist on rootfs (not orphaned)
        if files.contains_key(path) {
            continue;
        }

        for (component_id, fi) in entries {
            // skip ghost files
            if fi.flags.is_ghost() {
                continue;
            }

            // skip empty files; those are too common and may cause false positives
            if fi.size == 0 {
                continue;
            }

            let digest = match &fi.digest {
                Some(d) if !d.is_empty() => d,
                _ => continue, // directories, symlinks, or empty digest
            };

            digest_to_ids
                .entry(digest.clone())
                .or_default()
                .push(*component_id);
            digest_to_size.insert(digest.clone(), fi.size);
        }
    }

    // second pass: remove ambiguous digests and build final index
    let mut orphan_digest_index: HashMap<String, ComponentId> = HashMap::new();
    let mut orphan_sizes: HashSet<u64> = HashSet::new();
    let mut ambiguous_count: usize = 0;

    for (digest, ids) in digest_to_ids {
        let unique_ids: HashSet<ComponentId> = ids.into_iter().collect();
        if unique_ids.len() > 1 {
            ambiguous_count += 1;
            tracing::trace!(digest = %digest, components = unique_ids.len(), "excluding ambiguous orphan digest");
            continue;
        }
        // SAFETY: by construction in the previous loop, we're guaranteed to
        // have at least one id and an associated size
        let id = unique_ids.into_iter().next().unwrap();
        let size = digest_to_size.get(&digest).unwrap();
        orphan_digest_index.insert(digest, id);
        orphan_sizes.insert(*size);
    }

    tracing::debug!(
        index_size = orphan_digest_index.len(),
        unique_sizes = orphan_sizes.len(),
        ambiguous_excluded = ambiguous_count,
        "built orphan digest index"
    );

    repo.orphan_digest_index = orphan_digest_index;
    repo.orphan_sizes = orphan_sizes;
}

/// Compute the SHA-256 digest of a file in the rootfs.
fn compute_sha256(rootfs: &Dir, path: &Utf8Path) -> Result<String> {
    let rel_path = path.strip_prefix("/").unwrap_or(path.as_ref());
    let mut file = rootfs
        .open(rel_path.as_str())
        .with_context(|| format!("opening {path} for hashing"))?;

    let mut hasher = Hasher::new(MessageDigest::sha256()).context("creating SHA-256 hasher")?;
    std::io::copy(&mut file, &mut hasher).with_context(|| format!("hashing {path}"))?;

    let digest = hasher.finish().context("finalizing SHA-256 hash")?;
    Ok(hex::encode(digest))
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
    use rpm_qa::Package;

    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/fedora.qf");

    fn fi(file_type: FileType) -> crate::components::FileInfo {
        crate::components::FileInfo::dummy(file_type)
    }

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
        let claims =
            repo.strong_claims_for_path(Utf8Path::new("/usr/bin/bash"), &fi(FileType::File));
        assert_eq!(claims.len(), 1);
        let info = repo.component_info(claims[0]);
        assert_eq!(info.name, "bash");
        assert_eq!(info.mtime_clamp, 1753299195);

        // /usr/bin/sh is a symlink owned by bash
        let claims =
            repo.strong_claims_for_path(Utf8Path::new("/usr/bin/sh"), &fi(FileType::Symlink));
        assert_eq!(claims.len(), 1);
        let info = repo.component_info(claims[0]);
        assert_eq!(info.name, "bash");

        // /usr/lib64/libc.so.6 is a file owned by glibc
        let claims =
            repo.strong_claims_for_path(Utf8Path::new("/usr/lib64/libc.so.6"), &fi(FileType::File));
        assert_eq!(claims.len(), 1);
        let info = repo.component_info(claims[0]);
        assert_eq!(info.name, "glibc");
        assert_eq!(info.mtime_clamp, 1771428496);

        // Unowned file should not be claimed
        let claims =
            repo.strong_claims_for_path(Utf8Path::new("/some/unowned/file"), &fi(FileType::File));
        assert!(claims.is_empty());

        // RPMDB paths should not be claimed even if technically owned by rpm package
        for rpmdb_path in [
            "/usr/lib/sysimage/rpm/rpmdb.sqlite",
            "/usr/share/rpm/macros",
            "/var/lib/rpm/Packages",
        ] {
            let claims =
                repo.strong_claims_for_path(Utf8Path::new(rpmdb_path), &fi(FileType::File));
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
        let claims =
            repo.strong_claims_for_path(Utf8Path::new("/usr/bin/bash"), &fi(FileType::Symlink));
        assert!(claims.is_empty());

        // /usr/bin/sh is a symlink in RPM, but we query as file
        let claims = repo.strong_claims_for_path(Utf8Path::new("/usr/bin/sh"), &fi(FileType::File));
        assert!(claims.is_empty());
    }

    #[test]
    fn test_shared_directories_claimed_by_multiple_components() {
        let packages = rpm_qa::load_from_str(FIXTURE).unwrap();
        let repo = RpmRepo::load_from_packages(packages, now_secs()).unwrap();

        // /usr/lib/.build-id is a well-known directory shared by many packages
        let claims = repo.strong_claims_for_path(
            Utf8Path::new("/usr/lib/.build-id"),
            &fi(FileType::Directory),
        );
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
        let claims = repo.strong_claims_for_path(Utf8Path::new("/"), &fi(FileType::Directory));
        assert!(!claims.is_empty(), "/ should be claimed");
        assert_eq!(repo.component_info(claims[0]).name, "filesystem");

        let claims = repo.strong_claims_for_path(Utf8Path::new("/etc"), &fi(FileType::Directory));
        assert!(!claims.is_empty(), "/etc should be claimed");
        // /etc is owned by filesystem
        assert_eq!(repo.component_info(claims[0]).name, "filesystem");

        let claims = repo.strong_claims_for_path(Utf8Path::new("/etc/passwd"), &fi(FileType::File));
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

        let stability = calculate_stability(&changelog_times, buildtime, now);
        assert_eq!(stability, 0.99);
    }

    #[test]
    fn test_calculate_stability_very_recent() {
        // Package built within 1 day should return 0.0
        let now = now_secs();
        let recent_time = now - 3600; // 1 hour ago
        let changelog_times = vec![recent_time];
        let buildtime = recent_time;

        let stability = calculate_stability(&changelog_times, buildtime, now);
        assert_eq!(stability, 0.0);
    }

    #[test]
    fn test_calculate_stability_no_changelog_uses_buildtime() {
        use crate::components::SECS_PER_DAY;

        // No changelog entries should use buildtime as fallback
        let now = now_secs();
        let buildtime = now - (30 * SECS_PER_DAY); // 30 days ago
        let changelog_times: Vec<u64> = vec![];

        let stability = calculate_stability(&changelog_times, buildtime, now);
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

        let stability = calculate_stability(&changelog_times, buildtime, now);
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

        let stability = calculate_stability(&changelog_times, buildtime, now);
        assert_stability_in_range(stability, 0.0, 0.10);
    }

    #[test]
    fn test_stability_min_across_subpackages() {
        use std::collections::BTreeMap;

        // Two binary packages from the same SRPM but with different changelogs.
        // This simulates a compose bug where a noarch subpackage is from an
        // older build than the arch-specific one.
        let now = 1_800_000_000;
        let srpm = "foo-1.0-1.fc40.src.rpm";

        let foo = rpm_qa::Package {
            name: "foo".into(),
            version: "1.0".into(),
            release: "1.fc40".into(),
            epoch: None,
            arch: "x86_64".into(),
            license: "MIT".into(),
            size: 1000,
            buildtime: now - 200000,
            installtime: now,
            sourcerpm: Some(srpm.into()),
            digest_algo: None,
            changelog_times: vec![now - 200000, now - 300000],
            files: BTreeMap::new(),
        };

        // "foo2" has fresher changelogs so should have lower stability
        let mut foo2 = foo.clone();
        foo2.name = "foo2".into();
        foo2.changelog_times = vec![now, now - 100000];

        let stab_foo = calculate_stability(&foo.changelog_times, foo.buildtime, now);
        let stab_foo2 = calculate_stability(&foo2.changelog_times, foo2.buildtime, now);
        assert!(stab_foo > stab_foo2, "foo isn't more stable than foo2");

        let assert_stability = |first: &Package, second: &Package| {
            let mut packages: rpm_qa::Packages = HashMap::new();
            packages.insert(first.name.to_string(), first.clone());
            packages.insert(second.name.to_string(), second.clone());
            let repo = RpmRepo::load_from_packages(packages, now).unwrap();
            let info = repo.component_info(ComponentId(0));
            assert_eq!(info.name, "foo");
            // the component should use the min (most pessimistic) stability
            assert_eq!(info.stability, stab_foo2);
        };

        // try both orders
        assert_stability(&foo, &foo2);
        assert_stability(&foo2, &foo);
    }

    #[test]
    fn test_compute_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

        rootfs.write("hello.txt", "hello world\n").unwrap();
        let digest = compute_sha256(&rootfs, Utf8Path::new("/hello.txt")).unwrap();
        assert_eq!(
            digest,
            "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
        );

        rootfs.write("empty", "").unwrap();
        let digest = compute_sha256(&rootfs, Utf8Path::new("/empty")).unwrap();
        assert_eq!(
            digest,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_weak_claims_for_path() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

        // Write a file at a "moved" path. The rootfs has no /usr/bin/bash, so
        // that entry in the fixture is orphaned. We edit its digest to match
        // our moved file so the digest matching reclaims it.
        rootfs.create_dir("moved").unwrap();
        let content = "x".repeat(1024);
        rootfs.write("moved/bash", &content).unwrap();

        let files = crate::scan::Scanner::new(&rootfs).scan().unwrap();
        let digest = compute_sha256(&rootfs, Utf8Path::new("/moved/bash")).unwrap();
        let size = files.get(Utf8Path::new("/moved/bash")).unwrap().size;

        let mut packages = rpm_qa::load_from_str(FIXTURE).unwrap();
        let bash = packages.get_mut("bash").unwrap();
        let bash_fi = bash.files.get_mut(Utf8Path::new("/usr/bin/bash")).unwrap();
        bash_fi.digest = Some(digest);
        bash_fi.size = size;

        let mut repo = RpmRepo::load_from_packages(packages, now_secs()).unwrap();
        build_orphan_digest_index(&mut repo, &files);

        // the moved file should be reclaimed by bash's SRPM
        let moved_fi = files.get(Utf8Path::new("/moved/bash")).unwrap();
        let claims = repo
            .weak_claims_for_path(&rootfs, Utf8Path::new("/moved/bash"), moved_fi)
            .unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(repo.component_info(claims[0]).name, "bash");
    }

    #[test]
    fn test_weak_claims_for_path_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

        // Write a file and set two packages from different SRPMs to have the
        // same orphaned digest. We use /usr/bin/bash from bash and
        // /usr/lib64/libc.so.6 from glibc.
        let content = "x".repeat(1024);
        rootfs.write("data.bin", &content).unwrap();

        let files = crate::scan::Scanner::new(&rootfs).scan().unwrap();
        let digest = compute_sha256(&rootfs, Utf8Path::new("/data.bin")).unwrap();
        let size = files.get(Utf8Path::new("/data.bin")).unwrap().size;

        let mut packages = rpm_qa::load_from_str(FIXTURE).unwrap();

        let bash = packages.get_mut("bash").unwrap();
        let bash_fi = bash.files.get_mut(Utf8Path::new("/usr/bin/bash")).unwrap();
        bash_fi.digest = Some(digest.clone());
        bash_fi.size = size;

        let glibc = packages.get_mut("glibc").unwrap();
        let glibc_fi = glibc
            .files
            .get_mut(Utf8Path::new("/usr/lib64/libc.so.6"))
            .unwrap();
        glibc_fi.digest = Some(digest);
        glibc_fi.size = size;

        let mut repo = RpmRepo::load_from_packages(packages, now_secs()).unwrap();
        build_orphan_digest_index(&mut repo, &files);

        // ambiguous digest should not claim anything
        let data_fi = files.get(Utf8Path::new("/data.bin")).unwrap();
        let claims = repo
            .weak_claims_for_path(&rootfs, Utf8Path::new("/data.bin"), data_fi)
            .unwrap();
        assert!(claims.is_empty(), "ambiguous digest should not claim");
    }
}
