mod bigfiles;
mod rpm;
mod xattr;

use std::collections::{BTreeMap, HashMap};

/// The name of the component for files not claimed by any repo.
pub const UNCLAIMED_COMPONENT: &str = "chunkah/unclaimed";

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::cap_std::fs::{Dir, FileType as CapFileType, Metadata, MetadataExt};

use crate::utils;

/// Seconds per day.
pub const SECS_PER_DAY: u64 = 60 * 60 * 24;

/// Period in days for calculating stability probability.
/// TODO: make this configurable via CLI
pub const STABILITY_PERIOD_DAYS: f64 = 7.0;

/// Maximum lookback period in days for changelog analysis.
pub const STABILITY_LOOKBACK_DAYS: u64 = 365;

/// Loaded component repos along with the default mtime to use.
pub struct ComponentsRepos {
    repos: Vec<Box<dyn ComponentsRepo>>,
    default_mtime_clamp: u64,
}

/// Files belonging to a component.
#[derive(Debug, Clone)]
pub struct Component {
    /// The maximum mtime for files in this component during the build phase.
    /// File mtimes will be clamped to this value.
    pub mtime_clamp: u64,
    /// Probability that the component doesn't change over STABILITY_PERIOD_DAYS.
    /// Used by the packing algorithm.
    pub stability: f64,
    /// The files belonging to this component, with their metadata.
    pub files: FileMap,
}

/// A map from file paths to their metadata.
pub type FileMap = BTreeMap<Utf8PathBuf, FileInfo>;

/// Cached file metadata from the scan.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub file_type: FileType,
    pub mode: u32,
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u64,
    pub ino: u64,
    pub nlink: u64,
    pub xattrs: Vec<(String, Vec<u8>)>,
}

/// File type for entries in the rootfs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Directory,
    File,
    Symlink,
}

impl FileType {
    /// Try to convert from cap_std file type.
    ///
    /// Returns `None` for unsupported types (sockets, FIFOs, block/char devices).
    pub fn from_cap_std(file_type: &CapFileType) -> Option<Self> {
        if file_type.is_dir() {
            Some(FileType::Directory)
        } else if file_type.is_file() {
            Some(FileType::File)
        } else if file_type.is_symlink() {
            Some(FileType::Symlink)
        } else {
            None
        }
    }
}

impl FileInfo {
    /// Create FileInfo from metadata and xattrs.
    pub fn from_metadata(
        metadata: &Metadata,
        file_type: FileType,
        xattrs: Vec<(String, Vec<u8>)>,
    ) -> Self {
        Self {
            file_type,
            mode: metadata.mode(),
            size: metadata.len(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mtime: metadata.mtime() as u64,
            ino: metadata.ino(),
            nlink: metadata.nlink(),
            xattrs,
        }
    }
}

impl ComponentsRepos {
    /// Detect and load all component repos present in the given rootfs.
    ///
    /// The `files` map is the set of paths in the rootfs. This avoids the xattr
    /// repo having to walk the rootfs again. The `default_mtime_clamp` will be
    /// used as the mtime clamp for components that don't have a reproducible
    /// clamp (e.g. xattr-claimed files, unclaimed files).
    pub fn load(rootfs: &Dir, files: &FileMap, default_mtime_clamp: u64) -> Result<Self> {
        let mut repos: Vec<Box<dyn ComponentsRepo>> = Vec::new();

        if let Some(repo) =
            xattr::XattrRepo::load(files, default_mtime_clamp).context("loading xattrs")?
        {
            tracing::info!(repo = "xattr", "loaded repo");
            repos.push(Box::new(repo));
        }

        if let Some(repo) =
            rpm::RpmRepo::load(rootfs, files, default_mtime_clamp).context("loading rpmdb")?
        {
            tracing::info!(repo = "rpm", "loaded repo");
            repos.push(Box::new(repo));
        }

        if let Some(repo) = bigfiles::BigfilesRepo::load(files, default_mtime_clamp) {
            tracing::info!(repo = "bigfiles", "loaded repo");
            repos.push(Box::new(repo));
        }

        // Other backends (e.g. deb, apk, pip, etc.) would go here...

        Ok(Self {
            repos,
            default_mtime_clamp,
        })
    }

    /// Returns true if no repos were loaded.
    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
    }

    /// Claim files from repos and return the mapping of component names to files.
    ///
    /// Repos are sorted by priority (lower values first) before processing.
    /// Higher priority repos "win" - if they claim a path, lower priority repos
    /// are not consulted for that path. All unclaimed paths go into a catch-all.
    pub fn into_components(
        mut self,
        rootfs: &Dir,
        files: FileMap,
    ) -> Result<HashMap<String, Component>> {
        let mut claims: HashMap<(usize, ComponentId), FileMap> = HashMap::new();

        // make sure they're in priority order
        self.repos.sort_by_key(|r| r.default_priority());
        for (idx, repo) in self.repos.iter().enumerate() {
            tracing::trace!(name = %repo.name(), repo_idx = idx, "repo prioritized");
        }

        // all files start unclaimed
        let unclaimed = files;

        // check for strong path claims, then weak path claims on leftovers
        let unclaimed = claim_pass(
            rootfs,
            &self.repos,
            unclaimed,
            &mut claims,
            ClaimStrength::Strong,
        )
        .context("strong claims pass")?;
        let unclaimed = claim_pass(
            rootfs,
            &self.repos,
            unclaimed,
            &mut claims,
            ClaimStrength::Weak,
        )
        .context("weak claims pass")?;

        #[derive(Default)]
        struct RepoStats {
            components: usize,
            total_size: u64,
        }

        // build final components map, tracking per-repo stats
        let mut repo_stats: BTreeMap<usize, RepoStats> = BTreeMap::new();
        let mut components = HashMap::new();
        for ((repo_idx, comp_id), files) in claims {
            let repo = &self.repos[repo_idx];
            let info = repo.component_info(comp_id);
            let full_name = format!("{}/{}", repo.name(), info.name);
            let stats = repo_stats.entry(repo_idx).or_default();
            stats.components += 1;
            stats.total_size += files.values().map(|f| f.size).sum::<u64>();
            components.insert(
                full_name,
                Component {
                    mtime_clamp: info.mtime_clamp,
                    stability: info.stability,
                    files,
                },
            );
        }

        // log per-repo summary
        for (repo_idx, stats) in &repo_stats {
            let repo_name = self.repos[*repo_idx].name();
            tracing::info!(repo = repo_name, components = stats.components, size = %utils::format_size(stats.total_size), "repo summary");
        }

        // and the catch-all component for anything still unclaimed
        if !unclaimed.is_empty() {
            let size: u64 = unclaimed.values().map(|f| f.size).sum();
            tracing::info!(files = unclaimed.len(), size = %utils::format_size(size), "unclaimed files");
            components.insert(
                UNCLAIMED_COMPONENT.into(),
                Component {
                    mtime_clamp: self.default_mtime_clamp,
                    stability: 0.0,
                    files: unclaimed,
                },
            );
        }

        // Final pass: fill in stability for components with 0.0 (xattr,
        // bigfiles, unclaimed). Use half the minimum non-zero stability so
        // they're considered less stable than any known component, but non-zero
        // so the packing algorithm can make meaningful TEV loss calculations.
        let min_stability = components
            .values()
            .map(|c| c.stability)
            .filter(|&s| s > 0.0)
            // SAFETY: somehow getting NaN is a logic error somewhere
            .min_by(|a, b| a.partial_cmp(b).unwrap());
        let fallback_stability = match min_stability {
            Some(min) => min / 2.0,
            None => {
                // All components have stability 0.0; no package manager data available
                tracing::warn!("no stability data available, packing may be suboptimal");
                0.5
            }
        };
        tracing::debug!(
            min_stability = min_stability,
            fallback = fallback_stability,
            "computed fallback stability"
        );
        for comp in components.values_mut() {
            if comp.stability == 0.0 {
                comp.stability = fallback_stability;
            }
        }

        Ok(components)
    }
}

/// Whether to use strong or weak claims in a claiming pass.
#[derive(Debug)]
enum ClaimStrength {
    Strong,
    Weak,
}

/// Run a claiming pass over all files, consulting each repo in priority order.
/// Returns the files that were not claimed.
fn claim_pass(
    rootfs: &Dir,
    repos: &[Box<dyn ComponentsRepo>],
    files: FileMap,
    claims: &mut HashMap<(usize, ComponentId), FileMap>,
    strength: ClaimStrength,
) -> Result<FileMap> {
    let mut unclaimed = FileMap::new();
    // This is O(files x repos), though really the number of active
    // repos at any time is incredibly small; in the common case, 1.
    for (path, file_info) in files {
        let mut claimed = false;
        for (repo_idx, repo) in repos.iter().enumerate() {
            let ids = match strength {
                ClaimStrength::Strong => Ok(repo.strong_claims_for_path(&path, &file_info)),
                ClaimStrength::Weak => repo.weak_claims_for_path(rootfs, &path, &file_info),
            }
            .with_context(|| format!("claiming {path}"))?;
            if !ids.is_empty() {
                tracing::trace!(path = %path, repo_idx, ids = ?ids, ?strength, "path claimed");
                for id in ids {
                    claims
                        .entry((repo_idx, id))
                        .or_default()
                        .insert(path.clone(), file_info.clone());
                }
                claimed = true;
                break;
            }
        }
        if !claimed {
            tracing::trace!(path = %path, ?strength, "path unclaimed after pass");
            unclaimed.insert(path, file_info);
        }
    }
    Ok(unclaimed)
}

/// Opaque identifier for a component within a repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ComponentId(usize);

/// Information about a component.
struct ComponentInfo<'a> {
    pub name: &'a str,
    pub mtime_clamp: u64,
    pub stability: f64,
}

/// A trait for any type of "repo" of components (e.g., rpm, dpkg, etc.)
///
/// Components repos are query objects that answer which components claim a path.
trait ComponentsRepo {
    /// Returns the name of this repo type (e.g., "rpm", "xattr").
    fn name(&self) -> &'static str;

    /// Returns the priority of this repo for ordering purposes.
    ///
    /// Lower values indicate higher priority. Used to determine the order in
    /// which repos are queried. Higher priority repos "win" - if they claim a
    /// path, lower priority repos are not consulted. We could make the default
    /// overridable on the CLI in the future.
    fn default_priority(&self) -> usize;

    /// Query which components strongly claim this path.
    ///
    /// Strong path claims are authoritative claims; i.e. the system itself for
    /// example may have a database (like the rpmdb) which says that this path
    /// belongs in a specific component. Must be cheap (no I/O).
    ///
    /// Returns a list of component IDs that claim this path. For most paths,
    /// this returns 0 or 1 ID. Directories shared by multiple packages may
    /// return multiple IDs.
    ///
    /// Default implementation returns no claims.
    fn strong_claims_for_path(&self, _path: &Utf8Path, _file_info: &FileInfo) -> Vec<ComponentId> {
        vec![]
    }

    /// Query which components weakly claim this path.
    ///
    /// Weak path claims are best-effort claims; these have lower precedence
    /// than strong claims but still generally result in a more efficient
    /// outcome than leaving paths unclaimed. They may involve calculations
    /// (e.g. hashing) and heuristics.
    ///
    /// Default implementation returns no claims.
    fn weak_claims_for_path(
        &self,
        _rootfs: &Dir,
        _path: &Utf8Path,
        _file_info: &FileInfo,
    ) -> Result<Vec<ComponentId>> {
        Ok(vec![])
    }

    /// Get info about a component by ID.
    fn component_info(&self, id: ComponentId) -> ComponentInfo<'_>;
}

#[cfg(test)]
impl FileInfo {
    /// Create a dummy FileInfo with the given file type for tests.
    fn dummy(file_type: FileType) -> Self {
        Self {
            file_type,
            mode: 0,
            size: 0,
            uid: 0,
            gid: 0,
            mtime: 0,
            ino: 0,
            nlink: 1,
            xattrs: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use camino::Utf8Path;
    use cap_std_ext::cap_std::ambient_authority;
    use cap_std_ext::dirext::CapStdExtDirExt;

    use super::*;

    const RPM_FIXTURE: &str = include_str!("../../tests/fixtures/fedora.qf");

    const XATTR_NAME: &str = "user.component";

    #[test]
    fn test_into_components() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

        rootfs.create_dir_all("usr/bin").unwrap();
        rootfs.create_dir_all("usr/lib64").unwrap();
        rootfs.create_dir_all("usr/lib/sysimage/rpm").unwrap();
        rootfs.create_dir_all("opt/myapp").unwrap();
        rootfs.write("usr/bin/bash", "fake bash").unwrap();
        rootfs.write("usr/lib64/libc.so.6", "fake libc").unwrap();
        rootfs
            .write("usr/lib/sysimage/rpm/rpmdb.sqlite", "fake rpmdb")
            .unwrap();
        rootfs.write("opt/myapp/config", "config").unwrap();
        rootfs.write("opt/myapp/data", "data").unwrap();

        // set xattr on /usr/bin/bash to claim it for "xattr-component"
        rootfs
            .setxattr("usr/bin/bash", XATTR_NAME, b"xattr-component")
            .unwrap();

        // set xattr on /opt/myapp/data to claim it for "myapp"
        rootfs
            .setxattr("opt/myapp/data", XATTR_NAME, b"myapp")
            .unwrap();

        let files = crate::scan::Scanner::new(&rootfs).scan().unwrap();

        let xattr_repo = xattr::XattrRepo::load(&files, 0).unwrap().unwrap();
        let packages = rpm_qa::load_from_str(RPM_FIXTURE).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let rpm_repo = rpm::RpmRepo::load_from_packages(packages, now).unwrap();

        let repos: Vec<Box<dyn ComponentsRepo>> = vec![Box::new(rpm_repo), Box::new(xattr_repo)];
        let loaded = ComponentsRepos {
            repos,
            default_mtime_clamp: 0,
        };

        let components = loaded.into_components(&rootfs, files).unwrap();

        // example xattr overrides rpm entry
        assert!(
            components["xattr/xattr-component"]
                .files
                .contains_key(Utf8Path::new("/usr/bin/bash")),
            "/usr/bin/bash should belong to xattr/xattr-component"
        );

        // example rpm entry
        assert!(
            components["rpm/glibc"]
                .files
                .contains_key(Utf8Path::new("/usr/lib64/libc.so.6")),
            "/usr/lib64/libc.so.6 should belong to rpm/glibc"
        );

        // example xattr entry
        assert!(
            components["xattr/myapp"]
                .files
                .contains_key(Utf8Path::new("/opt/myapp/data")),
            "/opt/myapp/data should belong to xattr/myapp"
        );

        // example unclaimed entry
        assert!(
            components[UNCLAIMED_COMPONENT]
                .files
                .contains_key(Utf8Path::new("/opt/myapp/config")),
            "/opt/myapp/config should be unclaimed"
        );

        // rpmdb paths should be unclaimed
        assert!(
            components[UNCLAIMED_COMPONENT]
                .files
                .contains_key(Utf8Path::new("/usr/lib/sysimage/rpm/rpmdb.sqlite")),
            "/usr/lib/sysimage/rpm/rpmdb.sqlite should be unclaimed"
        );
    }

    #[test]
    fn test_into_components_xattr_only() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

        rootfs.create_dir_all("opt/myapp").unwrap();
        rootfs.write("opt/myapp/config", "config").unwrap();
        rootfs.setxattr("opt/myapp", XATTR_NAME, b"myapp").unwrap();

        let files = crate::scan::Scanner::new(&rootfs).scan().unwrap();

        let xattr_repo = xattr::XattrRepo::load(&files, 0).unwrap().unwrap();
        let repos: Vec<Box<dyn ComponentsRepo>> = vec![Box::new(xattr_repo)];
        let loaded = ComponentsRepos {
            repos,
            default_mtime_clamp: 0,
        };

        let components = loaded.into_components(&rootfs, files).unwrap();

        assert!(components.contains_key("xattr/myapp"));
        assert!(
            components["xattr/myapp"]
                .files
                .contains_key(Utf8Path::new("/opt/myapp/config"))
        );
    }
}
