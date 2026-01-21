use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;
use std::path::Path;

use anyhow::{Context, Result};
use camino::Utf8Path;
use cap_std::fs::Dir;
use cap_std_ext::dirext::{CapStdExtDirExt, WalkConfiguration};

use crate::components::{self, Component, FileInfo, FileMap, FileType};

/// Scan the rootfs for components and return a mapping of component names to components.
///
/// If `skip_special_files` is true, special file types (sockets, FIFOs,
/// block/char devices) are silently skipped. Otherwise, an error is returned.
pub fn scan_for_components(
    rootfs: &Dir,
    default_mtime_clamp: u64,
    skip_special_files: bool,
) -> Result<HashMap<String, Component>> {
    let files = scan_rootfs(rootfs, skip_special_files).context("scanning rootfs")?;

    let repos = components::ComponentsRepos::load(rootfs, &files, default_mtime_clamp)
        .context("loading components")?;
    if repos.is_empty() {
        // error for now but i guess we could just warn and have a single layer
        anyhow::bail!("no supported component repo found in rootfs");
    }

    Ok(repos.into_components(files))
}

/// Scan the rootfs and return a map of file paths to their metadata.
/// We use cap-std-ext's walk here, which doesn't follow symlinks.
///
/// If `skip_special_files` is true, special file types (sockets, FIFOs,
/// block/char devices) are silently skipped. Otherwise, an error is returned.
pub fn scan_rootfs(rootfs: &Dir, skip_special_files: bool) -> Result<FileMap> {
    let mut files = BTreeMap::new();

    let config = WalkConfiguration::default().path_base(Path::new("/"));

    rootfs
        .walk(&config, |component| {
            let path: &Utf8Path = component
                .path
                .try_into()
                .map_err(|_| anyhow::anyhow!("path is not valid UTF-8"))?;

            let rel_path = path.strip_prefix("/").unwrap_or(path);
            let fs_path = if rel_path.as_str().is_empty() {
                "."
            } else {
                rel_path.as_str()
            };

            let metadata = rootfs
                .symlink_metadata(fs_path)
                .with_context(|| format!("getting metadata for {}", path))?;

            // Check file type early, before reading xattrs
            let file_type = match FileType::from_cap_std(&metadata.file_type()) {
                Some(ft) => ft,
                None => {
                    if skip_special_files {
                        return Ok(ControlFlow::Continue(()));
                    } else {
                        anyhow::bail!("special file type not supported: {}", path);
                    }
                }
            };

            let xattrs = read_xattrs(rootfs, fs_path)
                .with_context(|| format!("reading xattrs for {}", path))?;

            let file_info = FileInfo::from_metadata(&metadata, file_type, xattrs);

            files.insert(path.to_owned(), file_info);
            Ok::<_, anyhow::Error>(ControlFlow::Continue(()))
        })
        .context("failed to walk rootfs")?;

    Ok(files)
}

/// Read all xattrs for a path.
pub fn read_xattrs(rootfs: &Dir, fs_path: &str) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    use std::ffi::OsStr;

    let xattr_list = rootfs
        .listxattrs(fs_path)
        .with_context(|| format!("listing xattrs for {}", fs_path))?;

    let mut xattrs = Vec::new();
    for key in xattr_list.iter() {
        // Skip selinux attributes for now. It would only bloat images since
        // _every_ file has SELinux attributes but they come from the container
        // runtime, not the tar layer, which is ignored. Bootable containers
        // could use them, but don't currently. We can make it opt in once it's
        // desirable.
        if key == OsStr::new("security.selinux") {
            continue;
        }

        if let Some(value) = rootfs
            .getxattr(fs_path, key)
            .with_context(|| format!("reading xattr {} for {}", key.display(), fs_path))?
        {
            // Technically, keeping the key as OsStr would be more correct,
            // but we'll need UTF-8 to shove it in a PAX header anyway so might
            // as well error now. Note libarchive and GNU tar differ here.
            // libarchive does urlencoding, GNU tar just writes the key as is
            // anyway. We'll cross that bridge when/if we get to it.
            let key_str = key
                .to_str()
                .with_context(|| format!("non-UTF8 xattr key {} on {}", key.display(), fs_path))?;
            xattrs.push((key_str.to_string(), value));
        }
    }

    Ok(xattrs)
}

#[cfg(test)]
mod tests {
    use camino::Utf8Path;
    use cap_std::ambient_authority;

    use super::*;
    use crate::components::FileType;

    /// Helper to get the file type for a path.
    fn get_file_type(files: &FileMap, path: &str) -> Option<FileType> {
        files.get(Utf8Path::new(path)).map(|f| f.file_type)
    }

    #[test]
    fn test_scan_rootfs_does_not_follow_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

        rootfs.create_dir("realdir").unwrap();
        rootfs.write("realdir/file.txt", "content").unwrap();
        rootfs.symlink("realdir", "linkdir").unwrap();
        rootfs.symlink("enoent", "broken").unwrap();
        rootfs.symlink("../../../etc/passwd", "escape").unwrap();

        let files = scan_rootfs(&rootfs, false).unwrap();

        assert_eq!(get_file_type(&files, "/realdir"), Some(FileType::Directory));
        assert_eq!(
            get_file_type(&files, "/realdir/file.txt"),
            Some(FileType::File)
        );

        assert_eq!(get_file_type(&files, "/linkdir"), Some(FileType::Symlink));
        assert_eq!(get_file_type(&files, "/broken"), Some(FileType::Symlink));
        assert_eq!(get_file_type(&files, "/escape"), Some(FileType::Symlink));
    }

    #[test]
    fn test_scan_rootfs_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

        let files = scan_rootfs(&rootfs, false).unwrap();

        // Should be empty. Note even the root directory is not included.
        // Root entries are not commonly in the tar stream. Container
        // runtimes ignore them so we may not even have read the real perms,
        // nor what we emit will be read. Bootable containers and other
        // OCI-but-not-container-runtime users could make use of them, but we'll
        // probably want to make it opt in if the use case shows up.
        assert_eq!(files.len(), 0);
    }

    #[test]
    fn test_scan_rootfs_nested_structure() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

        rootfs.create_dir_all("a/b/c").unwrap();
        rootfs.write("a/b/c/file", "content").unwrap();

        let files = scan_rootfs(&rootfs, false).unwrap();

        assert_eq!(get_file_type(&files, "/a"), Some(FileType::Directory));
        assert_eq!(get_file_type(&files, "/a/b"), Some(FileType::Directory));
        assert_eq!(get_file_type(&files, "/a/b/c"), Some(FileType::Directory));
        assert_eq!(get_file_type(&files, "/a/b/c/file"), Some(FileType::File));
    }

    #[test]
    fn test_scan_rootfs_special_file_type() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

        // Create a regular file and a Unix socket (special file type)
        rootfs.write("regular.txt", "content").unwrap();
        let socket_path = tmp.path().join("test.sock");
        let _socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();

        // By default, special file types should error
        let result = scan_rootfs(&rootfs, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_chain = format!("{:#}", err);
        assert!(
            err_chain.contains("special file type"),
            "expected 'special file type' in error chain, got: {}",
            err_chain
        );

        // With skip_special_files=true, the socket should be skipped
        let files = scan_rootfs(&rootfs, true).unwrap();

        // Regular file should be present
        assert_eq!(get_file_type(&files, "/regular.txt"), Some(FileType::File));

        // Socket should be skipped (not in the map)
        assert!(files.get(Utf8Path::new("/test.sock")).is_none());
    }
}
