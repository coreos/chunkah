use std::collections::HashMap;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::cap_std::fs::Dir;
use indexmap::IndexMap;
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
}
