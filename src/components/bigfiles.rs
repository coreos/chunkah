use std::collections::HashMap;

use camino::{Utf8Path, Utf8PathBuf};
use indexmap::IndexSet;

use super::{ComponentId, ComponentInfo, ComponentsRepo, FileMap, FileType};

/// Minimum file size in bytes to be considered a "big file" (1 MB).
const MIN_SIZE: u64 = 1024 * 1024;

const REPO_NAME: &str = "bigfiles";

/// Big files component repo implementation.
///
/// Claims any file larger than 1 MB into separate standalone components. This
/// solves a conceptual issue in the unclaimed files logic: by grouping together
/// all those files, they can't ever be broken back out into separate layers;
/// the packer considers each component as one monolithic unit. By breaking them
/// out, the packer can choose to merge them back in (or leave them separate)
/// as it sees fit. Conceptually, every unclaimed file should be considered
/// separately, but it's overkill to be this granular so we just filter for >1M.
///
/// Some special handling for hardlinked files (same inode); we still want
/// unclaimed files that are hardlinked to end up in the same component.
pub struct BigfilesRepo {
    /// Component names, indexed by ComponentId.
    components: IndexSet<String>,
    /// Mapping from path to ComponentId.
    path_to_component: HashMap<Utf8PathBuf, ComponentId>,
    /// Default mtime clamp for components.
    default_mtime_clamp: u64,
}

impl BigfilesRepo {
    /// Load bigfiles repo by scanning for files >= MIN_SIZE.
    ///
    /// Returns None if no qualifying files are found. Hardlinked files (same
    /// inode, nlink > 1) are grouped into the same component.
    pub fn load(files: &FileMap, default_mtime_clamp: u64) -> Option<Self> {
        let mut components: IndexSet<String> = IndexSet::new();
        let mut path_to_component: HashMap<Utf8PathBuf, ComponentId> = HashMap::new();

        // build inode table for hardlink handling
        let mut inode_to_paths: HashMap<u64, Vec<&Utf8PathBuf>> = HashMap::new();
        for (path, file_info) in files {
            if file_info.file_type == FileType::File
                && file_info.nlink > 1
                && file_info.size >= MIN_SIZE
            {
                inode_to_paths.entry(file_info.ino).or_default().push(path);
            }
        }

        for (path, file_info) in files {
            if file_info.file_type != FileType::File || file_info.size < MIN_SIZE {
                continue;
            }

            // skip if this inode was already processed via an earlier hardlink
            if file_info.nlink > 1 && !inode_to_paths.contains_key(&file_info.ino) {
                continue;
            }

            let filename = path
                .file_name()
                .map(|s| s.to_string())
                .expect("filename has no basename");

            // derive component name from filename
            let component_name = if components.contains(&filename) {
                // filename already used, use full path without leading '/'
                path.strip_prefix("/")
                    .expect("non-absolute file path in FileMap")
                    .as_str()
                    .to_string()
            } else {
                filename
            };

            // create a component for this path
            let (idx, _) = components.insert_full(component_name);
            tracing::trace!(path = %path, component = %components[idx], id = idx, "bigfile component created");
            let component_id = ComponentId(idx);
            path_to_component.insert(path.clone(), component_id);

            // if it has hardlinks, also shove those into the same component
            if let Some(linked_paths) = inode_to_paths.remove(&file_info.ino) {
                for linked_path in linked_paths {
                    path_to_component.insert(linked_path.clone(), component_id);
                }
            }
        }

        if components.is_empty() {
            return None;
        }

        tracing::debug!(
            components = components.len(),
            paths = path_to_component.len(),
            "loaded bigfiles components"
        );

        Some(Self {
            components,
            path_to_component,
            default_mtime_clamp,
        })
    }
}

impl ComponentsRepo for BigfilesRepo {
    fn name(&self) -> &'static str {
        REPO_NAME
    }

    fn default_priority(&self) -> usize {
        80
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
            stability: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use cap_std_ext::cap_std::ambient_authority;
    use cap_std_ext::cap_std::fs::Dir;

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

    /// Helper to create a sparse file with the given size (uses no disk space).
    fn create_sparse_file(rootfs: &Dir, path: &str, size: u64) {
        let file = rootfs.create(path).unwrap();
        file.set_len(size).unwrap();
    }

    /// Helper to assert a path is claimed by a specific component.
    fn assert_component(repo: &BigfilesRepo, path: &str, expected: &str) {
        let claims = repo.claims_for_path(Utf8Path::new(path), FileType::File);
        assert_eq!(claims.len(), 1, "{path} should have exactly one claim");
        assert_eq!(
            repo.component_info(claims[0]).name,
            expected,
            "{path} should be claimed by {expected}"
        );
    }

    #[test]
    fn test_bigfiles_claims_large_files() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

        rootfs.create_dir_all("usr/bin").unwrap();
        rootfs.write("usr/bin/small", "x").unwrap(); // < 1 MB

        rootfs.create_dir_all("usr/lib/modules").unwrap();
        create_sparse_file(&rootfs, "usr/lib/modules/initramfs.img", 118 * 1024 * 1024);

        rootfs
            .create_dir_all("usr/lib/sysimage/rpm-ostree-base-db")
            .unwrap();
        rootfs.create_dir_all("usr/share/rpm").unwrap();
        create_sparse_file(
            &rootfs,
            "usr/lib/sysimage/rpm-ostree-base-db/rpmdb.sqlite",
            26 * 1024 * 1024,
        );
        std::fs::hard_link(
            tmp.path()
                .join("usr/lib/sysimage/rpm-ostree-base-db/rpmdb.sqlite"),
            tmp.path().join("usr/share/rpm/rpmdb.sqlite"),
        )
        .unwrap();

        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();
        let files = crate::scan::Scanner::new(&rootfs).scan().unwrap();

        let repo = BigfilesRepo::load(&files, 12345).unwrap();

        // small file should not be claimed
        let claims = repo.claims_for_path(Utf8Path::new("/usr/bin/small"), FileType::File);
        assert!(claims.is_empty());

        // large files should be claimed with their filename
        assert_component(&repo, "/usr/lib/modules/initramfs.img", "initramfs.img");

        // hardlinked files should be claimed by the same component
        assert_component(
            &repo,
            "/usr/lib/sysimage/rpm-ostree-base-db/rpmdb.sqlite",
            "rpmdb.sqlite",
        );
        assert_component(&repo, "/usr/share/rpm/rpmdb.sqlite", "rpmdb.sqlite");

        // there should be exactly 2 components (initramfs.img + rpmdb.sqlite)
        assert_eq!(repo.components.len(), 2);
    }

    #[test]
    fn test_bigfiles_duplicate_filenames() {
        let (_tmp, files) = setup_rootfs(|rootfs| {
            rootfs.create_dir("a").unwrap();
            rootfs.create_dir("b").unwrap();

            create_sparse_file(rootfs, "a/foobar", 4 * 1024 * 1024);
            create_sparse_file(rootfs, "b/foobar", 4 * 1024 * 1024);
        });

        let repo = BigfilesRepo::load(&files, 0).unwrap();

        // First one uses filename, second uses full path
        assert_component(&repo, "/a/foobar", "foobar");
        assert_component(&repo, "/b/foobar", "b/foobar");
    }
}
