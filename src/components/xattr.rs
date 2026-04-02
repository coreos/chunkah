use std::collections::HashMap;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use indexmap::IndexSet;

use super::{
    ComponentId, ComponentInfo, ComponentsRepo, FileInfo, FileMap, FileType, STABILITY_PERIOD_DAYS,
};

const XATTR_NAME: &str = "user.component";
const UPDATE_INTERVAL_XATTR_NAME: &str = "user.update-interval";
const UPDATE_INTERVAL_DEFAULT: u64 = 7; // i.e. weekly
const REPO_NAME: &str = "xattr";

/// Xattr-based components repo implementation.
///
/// Uses the `user.component` extended attribute to determine file ownership.
/// Directories with this xattr apply to all files underneath unless overridden.
/// Directory inheritance is pre-computed during load.
pub struct XattrRepo {
    /// Component names, indexed by ComponentId.
    components: IndexSet<String>,
    /// Mapping from path to ComponentId (pre-computed with inheritance).
    path_to_component: HashMap<Utf8PathBuf, ComponentId>,
    /// Per-component stability, indexed by ComponentId.
    component_stability: Vec<f64>,
    /// Currently, the on-disk mtime is canonical and we clamp it, but it would
    /// make sense in the future to support another user xattr to specify a
    /// canonical mtime for easier layer reproducibility.
    default_mtime_clamp: u64,
}

impl XattrRepo {
    /// Load xattr repo by scanning rootfs for user.component xattrs.
    /// Pre-computes directory inheritance for all paths in `files`.
    /// Uses cached xattrs from FileInfo rather than reading from disk.
    pub fn load(files: &FileMap, default_mtime_clamp: u64) -> Result<Option<Self>> {
        let mut components: IndexSet<String> = IndexSet::new();
        let mut path_to_component: HashMap<Utf8PathBuf, ComponentId> = HashMap::new();
        // Track raw intervals during scanning to detect conflicts
        let mut update_intervals: Vec<Option<u64>> = Vec::new();

        // Track active directory components: (path, ComponentId)
        // Directories with xattrs apply their component to descendants
        let mut dir_stack: Vec<(&Utf8Path, ComponentId)> = Vec::new();

        for (path, file_info) in files {
            // Pop directories that are not ancestors of current path
            while let Some((dir_path, _)) = dir_stack.last() {
                if path.starts_with(dir_path) && path.as_path() != *dir_path {
                    break;
                }
                dir_stack.pop();
            }

            let own_xattr = get_component_xattr(file_info)
                .with_context(|| format!("reading xattr for {}", path))?;

            // If this path has an xattr, get or create its ComponentId
            let own_component_id = own_xattr.as_ref().map(|name| {
                // simplify this when we have either
                // https://github.com/indexmap-rs/indexmap/issues/355 or
                // https://github.com/indexmap-rs/indexmap/issues/388
                let idx = components.get_index_of(name).unwrap_or_else(|| {
                    let idx = components.insert_full(name.clone()).0;
                    update_intervals.push(None);
                    tracing::trace!(path = %path, name = %name, id = idx, "xattr component created");
                    idx
                });
                ComponentId(idx)
            });

            // If this directory has an xattr, push to stack for children to inherit
            if file_info.file_type == FileType::Directory
                && let Some(id) = own_component_id
            {
                dir_stack.push((path.as_path(), id));
            }

            // Determine effective component: own xattr or inherited from parent dir
            let effective_id = own_component_id.or_else(|| dir_stack.last().map(|(_, id)| *id));

            if let Some(id) = effective_id {
                tracing::trace!(path = %path, component_id = id.0, "xattr assignment");
                path_to_component.insert(path.clone(), id);

                // Check for user.update-interval xattr (not inherited from directories)
                if let Some(interval) = get_update_interval_xattr(file_info)
                    .with_context(|| format!("reading update interval xattr for {path}"))?
                {
                    let existing = &mut update_intervals[id.0];
                    match *existing {
                        Some(prev) if prev != interval => {
                            let name = &components[id.0];
                            anyhow::bail!(
                                "conflicting {UPDATE_INTERVAL_XATTR_NAME} values for component \
                                 {name}: {prev} and {interval} (at {path})"
                            );
                        }
                        Some(_) => {} // same value, no-op
                        None => {
                            *existing = Some(interval);
                            tracing::debug!(
                                path = %path,
                                component = &components[id.0],
                                interval_days = interval,
                                "update-interval xattr set"
                            );
                        }
                    }
                }
            }
        }

        if components.is_empty() {
            return Ok(None);
        }

        // Convert raw intervals to stability probabilities
        let component_stability: Vec<f64> = update_intervals
            .iter()
            .enumerate()
            .map(|(idx, interval)| match interval {
                Some(days) => interval_to_stability(*days),
                None => {
                    tracing::debug!(
                        component = &components[idx],
                        "no {UPDATE_INTERVAL_XATTR_NAME} set, defaulting to {UPDATE_INTERVAL_DEFAULT} days"
                    );
                    interval_to_stability(UPDATE_INTERVAL_DEFAULT)
                }
            })
            .collect();

        tracing::debug!(
            components = components.len(),
            paths = path_to_component.len(),
            "loaded xattr components"
        );

        Ok(Some(Self {
            components,
            path_to_component,
            component_stability,
            default_mtime_clamp,
        }))
    }
}

/// Extract the user.component xattr value from cached xattrs.
fn get_component_xattr(file_info: &FileInfo) -> Result<Option<String>> {
    get_xattr_string(file_info, XATTR_NAME)
}

/// Extract the user.update-interval xattr value as an interval in days.
fn get_update_interval_xattr(file_info: &FileInfo) -> Result<Option<u64>> {
    get_xattr_string(file_info, UPDATE_INTERVAL_XATTR_NAME)?
        .map(|v| parse_update_interval_xattr(&v))
        .transpose()
}

/// Extract a string xattr value from cached xattrs.
fn get_xattr_string(file_info: &FileInfo, name: &str) -> Result<Option<String>> {
    file_info
        .xattrs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| {
            String::from_utf8(v.clone())
                .map_err(|e| anyhow::anyhow!("invalid UTF-8 in {name} xattr: {e}"))
        })
        .transpose()
}

/// Parse a user.update-interval xattr value into an interval in days.
///
/// Accepts either a positive integer (number of days) or a named label:
/// daily (1), weekly (7), biweekly (14), monthly (30), quarterly (90),
/// yearly (365).
fn parse_update_interval_xattr(value: &str) -> Result<u64> {
    let interval = match value {
        "daily" => 1,
        "weekly" => 7,
        "biweekly" => 14,
        "monthly" => 30,
        "quarterly" => 90,
        "yearly" => 365,
        _ => value
            .parse::<u64>()
            .with_context(|| format!("invalid {UPDATE_INTERVAL_XATTR_NAME} value: {value:?}"))?,
    };
    anyhow::ensure!(
        interval > 0,
        "invalid {UPDATE_INTERVAL_XATTR_NAME} value: must be a positive integer, got {value:?}"
    );
    Ok(interval)
}

/// Convert a stability interval in days to a probability using the Poisson model.
fn interval_to_stability(interval_days: u64) -> f64 {
    (-STABILITY_PERIOD_DAYS / interval_days as f64).exp()
}

impl ComponentsRepo for XattrRepo {
    fn name(&self) -> &'static str {
        REPO_NAME
    }

    fn default_priority(&self) -> usize {
        0
    }

    fn strong_claims_for_path(
        &self,
        path: &Utf8Path,
        _file_info: &super::FileInfo,
    ) -> Vec<ComponentId> {
        self.path_to_component
            .get(path)
            .map(|id| vec![*id])
            .unwrap_or_default()
    }

    fn component_info(&self, id: ComponentId) -> ComponentInfo<'_> {
        ComponentInfo {
            name: self
                .components
                .get_index(id.0)
                // SAFETY: the ids we're given come from the IndexSet itself
                // when we inserted the element, so it must be valid.
                .expect("invalid ComponentId"),
            mtime_clamp: self.default_mtime_clamp,
            stability: self.component_stability[id.0],
        }
    }
}

#[cfg(test)]
mod tests {
    use cap_std_ext::cap_std::ambient_authority;
    use cap_std_ext::cap_std::fs::Dir;
    use cap_std_ext::dirext::CapStdExtDirExt;

    use super::*;

    /// Helper to set up a rootfs, run setup, and scan files.
    /// Returns (tempdir, files) - caller must keep tempdir alive.
    fn setup_rootfs<F>(setup: F) -> (tempfile::TempDir, FileMap)
    where
        F: FnOnce(&Dir),
    {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();
        setup(&rootfs);
        let files = crate::scan::Scanner::new(&rootfs).scan().unwrap();
        (tmp, files)
    }

    /// Helper to set the component xattr on a path.
    fn set_component(rootfs: &Dir, path: &str, component: &str) {
        rootfs
            .setxattr(path, XATTR_NAME, component.as_bytes())
            .unwrap();
    }

    /// Helper to set the update-interval xattr on a path.
    fn set_update_interval(rootfs: &Dir, path: &str, value: &str) {
        rootfs
            .setxattr(path, UPDATE_INTERVAL_XATTR_NAME, value.as_bytes())
            .unwrap();
    }

    fn fi(file_type: FileType) -> super::FileInfo {
        super::FileInfo::dummy(file_type)
    }

    /// Helper to assert a path is claimed by a specific component.
    fn assert_component(repo: &XattrRepo, path: &str, file_type: FileType, expected: &str) {
        let claims = repo.strong_claims_for_path(Utf8Path::new(path), &fi(file_type));
        assert_eq!(claims.len(), 1, "{path} should have exactly one claim");
        assert_eq!(
            repo.component_info(claims[0]).name,
            expected,
            "{path} should be claimed by {expected}"
        );
    }

    #[test]
    fn test_xattr_file_overrides_directory() {
        let (_tmp, files) = setup_rootfs(|rootfs| {
            // Create a directory with xattr
            rootfs.create_dir("mydir").unwrap();
            set_component(rootfs, "mydir", "dircomponent");

            // File with its own xattr overrides directory
            rootfs.write("mydir/special", "content").unwrap();
            set_component(rootfs, "mydir/special", "filecomponent");

            // File without xattr inherits from directory
            rootfs.write("mydir/normal", "content").unwrap();

            // File without xattr outside of directory - should not be claimed
            rootfs.write("noattr", "content").unwrap();
        });
        let repo = XattrRepo::load(&files, 0).unwrap().unwrap();

        // /mydir and /mydir/normal should be dircomponent
        assert_component(&repo, "/mydir", FileType::Directory, "dircomponent");
        assert_component(&repo, "/mydir/normal", FileType::File, "dircomponent");

        // /mydir/special should be filecomponent
        assert_component(&repo, "/mydir/special", FileType::File, "filecomponent");

        // /noattr should not be claimed
        let claims = repo.strong_claims_for_path(Utf8Path::new("/noattr"), &fi(FileType::File));
        assert!(claims.is_empty());
    }

    #[test]
    fn test_xattr_inheritance() {
        // Tests nested overrides and sibling isolation:
        // /a has xattr A, /a/b has xattr B (overrides A), /a/b/c/d has xattr D
        // /x has xattr X (sibling of /a, should not interfere)
        let (_tmp, files) = setup_rootfs(|rootfs| {
            rootfs.create_dir_all("a/b/c/d").unwrap();
            rootfs.write("a/other", "content").unwrap();
            rootfs.create_dir("x").unwrap();
            rootfs.write("x/file", "content").unwrap();

            set_component(rootfs, "a", "compA");
            set_component(rootfs, "a/b", "compB");
            set_component(rootfs, "a/b/c/d", "compD");
            set_component(rootfs, "x", "compX");
        });
        let repo = XattrRepo::load(&files, 0).unwrap().unwrap();

        assert_component(&repo, "/a", FileType::Directory, "compA");
        assert_component(&repo, "/a/other", FileType::File, "compA"); // inherits from /a
        assert_component(&repo, "/a/b", FileType::Directory, "compB");
        assert_component(&repo, "/a/b/c", FileType::Directory, "compB"); // inherits from /a/b
        assert_component(&repo, "/a/b/c/d", FileType::Directory, "compD"); // own xattr
        assert_component(&repo, "/x", FileType::Directory, "compX"); // sibling of /a
        assert_component(&repo, "/x/file", FileType::File, "compX"); // inherits from /x
    }

    #[test]
    fn test_xattr_symlink_inherits_from_parent() {
        // Symlinks don't support user xattrs, but they should inherit from parent directory
        let (_tmp, files) = setup_rootfs(|rootfs| {
            rootfs.create_dir("mydir").unwrap();
            set_component(rootfs, "mydir", "mycomp");

            // Create a symlink inside the directory - it should inherit from parent
            rootfs.symlink("../somewhere", "mydir/link").unwrap();
        });
        let repo = XattrRepo::load(&files, 0).unwrap().unwrap();

        // Both should be claimed by mycomp
        assert_component(&repo, "/mydir", FileType::Directory, "mycomp");
        assert_component(&repo, "/mydir/link", FileType::Symlink, "mycomp");
    }

    #[test]
    fn test_xattr_stability_named_labels() {
        // Test that named labels are parsed correctly
        assert_eq!(parse_update_interval_xattr("daily").unwrap(), 1);
        assert_eq!(parse_update_interval_xattr("weekly").unwrap(), 7);
        assert_eq!(parse_update_interval_xattr("biweekly").unwrap(), 14);
        assert_eq!(parse_update_interval_xattr("monthly").unwrap(), 30);
        assert_eq!(parse_update_interval_xattr("quarterly").unwrap(), 90);
        assert_eq!(parse_update_interval_xattr("yearly").unwrap(), 365);
    }

    #[test]
    fn test_xattr_stability_invalid_values() {
        assert!(parse_update_interval_xattr("0").is_err());
        assert!(parse_update_interval_xattr("-1").is_err());
        assert!(parse_update_interval_xattr("abc").is_err());
        assert!(parse_update_interval_xattr("3.5").is_err());
        assert!(parse_update_interval_xattr("").is_err());
    }

    #[test]
    fn test_xattr_stability_conflicting_values() {
        let (_tmp, files) = setup_rootfs(|rootfs| {
            rootfs.create_dir("mydir").unwrap();
            set_component(rootfs, "mydir", "mycomp");
            set_update_interval(rootfs, "mydir", "30");
            rootfs.write("mydir/file", "content").unwrap();
            set_update_interval(rootfs, "mydir/file", "60");
        });
        let result = XattrRepo::load(&files, 0);
        assert!(result.is_err());
        let msg = format!("{:#}", result.err().unwrap());
        assert!(msg.contains("conflicting"), "unexpected error: {msg}");
    }

    #[test]
    fn test_xattr_stability_agreeing_values() {
        let (_tmp, files) = setup_rootfs(|rootfs| {
            rootfs.create_dir("mydir").unwrap();
            set_component(rootfs, "mydir", "mycomp");
            set_update_interval(rootfs, "mydir", "monthly");
            rootfs.write("mydir/file", "content").unwrap();
            set_update_interval(rootfs, "mydir/file", "30");
        });
        let repo = XattrRepo::load(&files, 0).unwrap().unwrap();

        let claims = repo.strong_claims_for_path(Utf8Path::new("/mydir"), &fi(FileType::Directory));
        let expected = interval_to_stability(30);
        assert_eq!(repo.component_info(claims[0]).stability, expected);
    }

    #[test]
    fn test_xattr_stability_not_inherited() {
        // user.update-interval on one component shouldn't affect another
        let (_tmp, files) = setup_rootfs(|rootfs| {
            rootfs.create_dir("app1").unwrap();
            set_component(rootfs, "app1", "comp1");
            set_update_interval(rootfs, "app1", "yearly");
            rootfs.write("app1/file", "content").unwrap();

            rootfs.create_dir("app2").unwrap();
            set_component(rootfs, "app2", "comp2");
            rootfs.write("app2/file", "content").unwrap();
        });
        let repo = XattrRepo::load(&files, 0).unwrap().unwrap();

        // comp1 should have yearly stability
        let claims = repo.strong_claims_for_path(Utf8Path::new("/app1"), &fi(FileType::Directory));
        assert_eq!(
            repo.component_info(claims[0]).stability,
            interval_to_stability(365)
        );

        // comp2 should have the default (weekly)
        let claims = repo.strong_claims_for_path(Utf8Path::new("/app2"), &fi(FileType::Directory));
        assert_eq!(
            repo.component_info(claims[0]).stability,
            interval_to_stability(7)
        );
    }
}
