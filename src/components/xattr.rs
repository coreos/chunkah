use std::collections::HashMap;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use indexmap::IndexSet;

use super::{ComponentId, ComponentInfo, ComponentsRepo, FileInfo, FileMap, FileType};

const XATTR_NAME: &str = "user.component";
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
                let idx = components
                    .get_index_of(name)
                    .unwrap_or_else(|| components.insert_full(name.clone()).0);
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
                path_to_component.insert(path.clone(), id);
            }
        }

        if components.is_empty() {
            return Ok(None);
        }

        Ok(Some(Self {
            components,
            path_to_component,
            default_mtime_clamp,
        }))
    }
}

/// Extract the user.component xattr value from cached xattrs.
fn get_component_xattr(file_info: &FileInfo) -> Result<Option<String>> {
    file_info
        .xattrs
        .iter()
        .find(|(k, _)| k == XATTR_NAME)
        .map(|(_, v)| {
            String::from_utf8(v.clone())
                .map_err(|e| anyhow::anyhow!("invalid UTF-8 in {XATTR_NAME} xattr: {e}"))
        })
        .transpose()
}

impl ComponentsRepo for XattrRepo {
    fn name(&self) -> &'static str {
        REPO_NAME
    }

    fn default_priority(&self) -> usize {
        0
    }

    fn claims_for_path(&self, path: &Utf8Path, _file_type: FileType) -> Vec<ComponentId> {
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
            // TODO: make this configurable via xattr or CLI
            stability: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use cap_std::ambient_authority;
    use cap_std::fs::Dir;
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
        let files = crate::scan::scan_rootfs(&rootfs, false).unwrap();
        (tmp, files)
    }

    /// Helper to set the component xattr on a path.
    fn set_component(rootfs: &Dir, path: &str, component: &str) {
        rootfs
            .setxattr(path, XATTR_NAME, component.as_bytes())
            .unwrap();
    }

    /// Helper to assert a path is claimed by a specific component.
    fn assert_component(repo: &XattrRepo, path: &str, file_type: FileType, expected: &str) {
        let claims = repo.claims_for_path(Utf8Path::new(path), file_type);
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
        let claims = repo.claims_for_path(Utf8Path::new("/noattr"), FileType::File);
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
}
