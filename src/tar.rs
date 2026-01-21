use std::collections::HashMap;
use std::io::Write;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use ocidir::oci_spec::image as oci_image;
use ocidir::{BlobWriter, WriteComplete};

use crate::components::{FileInfo, FileMap, FileType};

/// Compression options for OCI archives.
pub enum ArchiveCompression {
    /// No compression.
    None,
    /// Gzip compression with the specified level.
    Gzip(flate2::Compression),
}

/// A passthrough writer that performs no compression.
// XXX: upstream this to ocidir as e.g. create_uncompressed_layer() ?
pub(crate) struct NoCompression<'a>(BlobWriter<'a>);

impl std::io::Write for NoCompression<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl<'a> WriteComplete<BlobWriter<'a>> for NoCompression<'a> {
    fn complete(self) -> std::io::Result<BlobWriter<'a>> {
        Ok(self.0)
    }
}

/// Layer writer that can be either compressed or uncompressed.
pub enum LayerWriter<'a> {
    Uncompressed(ocidir::LayerWriter<'a, NoCompression<'a>>),
    Gzip(ocidir::LayerWriter<'a, flate2::write::GzEncoder<BlobWriter<'a>>>),
}

impl<'a> Write for LayerWriter<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            LayerWriter::Uncompressed(w) => w.write(buf),
            LayerWriter::Gzip(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            LayerWriter::Uncompressed(w) => w.flush(),
            LayerWriter::Gzip(w) => w.flush(),
        }
    }
}

impl<'a> LayerWriter<'a> {
    /// Complete the layer and return the layer.
    pub fn complete(self) -> Result<ocidir::Layer> {
        match self {
            LayerWriter::Uncompressed(w) => w.complete().context("completing uncompressed layer"),
            LayerWriter::Gzip(w) => w.complete().context("completing gzip layer"),
        }
    }
}

/// Create a tar builder for a new layer in an OCI directory.
pub fn create_layer(
    oci_dir: &ocidir::OciDir,
    compression: crate::ocibuilder::Compression,
) -> Result<tar::Builder<LayerWriter<'_>>> {
    let layer_writer = match compression {
        crate::ocibuilder::Compression::None => {
            let layer_writer = oci_dir
                .create_custom_layer(|bw| Ok(NoCompression(bw)), oci_image::MediaType::ImageLayer)
                .context("creating uncompressed layer writer")?;
            LayerWriter::Uncompressed(layer_writer)
        }
        crate::ocibuilder::Compression::Gzip(level) => {
            let level = flate2::Compression::new(level);
            let layer_writer = oci_dir
                .create_custom_layer(
                    |bw| Ok(flate2::write::GzEncoder::new(bw, level)),
                    oci_image::MediaType::ImageLayerGzip,
                )
                .context("creating gzip layer writer")?;
            LayerWriter::Gzip(layer_writer)
        }
    };
    Ok(tar::Builder::new(layer_writer))
}

/// Build a tar layer from a list of files and return the completed layer.
///
/// Parent directories are automatically created as needed using metadata from
/// the files map. This uses a stack-based approach that leverages the sorted order
/// of the input BTreeMap for efficiency.
pub fn write_files_to_tar<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    rootfs: &cap_std::fs::Dir,
    files: &FileMap,
    mtime_clamp: u64,
) -> Result<()> {
    // Stack of written directory paths - leverages sorted iteration order
    let mut dir_stack: Vec<&Utf8Path> = Vec::new();
    // Track inode -> first path written for hardlink detection.
    let mut inode_to_path: HashMap<u64, Utf8PathBuf> = HashMap::new();

    for (path, file_info) in files {
        // Pop directories that are not ancestors of current path
        while let Some(top) = dir_stack.last() {
            if path.starts_with(top) && path.as_path() != *top {
                break;
            }
            dir_stack.pop();
        }

        // Collect ancestors that need to be written (between stack top and
        // current path's parent)
        let ancestors: Vec<_> = path
            .ancestors()
            .skip(1) // skip self
            .filter(|p| !p.as_str().is_empty() && *p != "/")
            .take_while(|p| dir_stack.last().is_none_or(|top| *p != *top))
            .collect();

        // Write ancestors in reverse order (shallowest first) and push to stack
        for ancestor in ancestors.into_iter().rev() {
            let ancestor_path = Utf8PathBuf::from(ancestor);
            // XXX: somehow reuse existing FileInfos for that dir, which may
            // live in other components
            let ancestor_info = if let Some(info) = files.get(&ancestor_path) {
                info.clone()
            } else {
                let rel_path = ancestor.strip_prefix("/").unwrap_or(ancestor);
                let metadata = rootfs
                    .symlink_metadata(rel_path)
                    .with_context(|| format!("getting metadata for {}", ancestor))?;
                let xattrs = crate::scan::read_xattrs(rootfs, rel_path.as_str())
                    .with_context(|| format!("reading xattrs for {}", ancestor))?;
                FileInfo::from_metadata(&metadata, FileType::Directory, xattrs)
            };
            write_dir_entry(tar_builder, ancestor, mtime_clamp, &ancestor_info)
                .with_context(|| format!("writing parent directory {}", ancestor))?;
            dir_stack.push(ancestor);
        }

        // Handle hardlinks up front
        if file_info.file_type != FileType::Directory && file_info.nlink > 1 {
            if let Some(first_path) = inode_to_path.get(&file_info.ino) {
                write_hardlink_entry(tar_builder, path, first_path, mtime_clamp, file_info)?;
                continue;
            }
            // First occurrence of this hardlinked file/symlink
            inode_to_path.insert(file_info.ino, path.clone());
        }

        match file_info.file_type {
            FileType::Directory => {
                write_dir_entry(tar_builder, path, mtime_clamp, file_info)?;
                // We might enter this directory in the next iteration; push it
                dir_stack.push(path.as_path());
            }
            FileType::File => {
                write_file_entry(tar_builder, rootfs, path, mtime_clamp, file_info)?;
            }
            FileType::Symlink => {
                write_symlink_entry(tar_builder, rootfs, path, mtime_clamp, file_info)?;
            }
        }
    }
    Ok(())
}

/// Write the OCI directory as a tar archive to a writer.
// XXX: Consider upstreaming this to ocidir-rs.
pub fn write_oci_archive<W: Write>(
    oci_dir: &cap_std::fs::Dir,
    writer: W,
    compression: ArchiveCompression,
) -> Result<()> {
    match compression {
        ArchiveCompression::None => write_oci_archive_to(oci_dir, writer),
        ArchiveCompression::Gzip(level) => {
            let gzip_writer = flate2::write::GzEncoder::new(writer, level);
            write_oci_archive_to(oci_dir, gzip_writer)
        }
    }
}

/// Strip leading "/" from a path, returning the path unchanged if no prefix.
fn strip_root_prefix(path: &Utf8Path) -> &Utf8Path {
    path.strip_prefix("/").unwrap_or(path)
}

/// Prepare a tar header with common metadata from FileInfo.
fn write_header_from_file_info(header: &mut tar::Header, file_info: &FileInfo, mtime_clamp: u64) {
    let mtime = std::cmp::min(file_info.mtime, mtime_clamp);
    header.set_mtime(mtime);
    header.set_uid(file_info.uid as u64);
    header.set_gid(file_info.gid as u64);
    header.set_mode(file_info.mode);
}

/// Append xattrs as PAX extensions to the tar stream.
///
/// This must be called before appending the actual file entry.
/// Uses the SCHILY.xattr.{key} format that tools like tar understand.
fn append_xattrs<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    xattrs: &[(String, Vec<u8>)],
    path: &str,
) -> Result<()> {
    if xattrs.is_empty() {
        return Ok(());
    }

    let pax_extensions: Vec<_> = xattrs
        .iter()
        .map(|(k, v)| (format!("SCHILY.xattr.{k}"), v.clone()))
        .collect();

    tar_builder
        .append_pax_extensions(
            pax_extensions
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_slice())),
        )
        .with_context(|| format!("appending xattrs for {}", path))?;

    Ok(())
}

/// Write a directory entry to the tar archive.
fn write_dir_entry<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    path: &Utf8Path,
    mtime_clamp: u64,
    file_info: &FileInfo,
) -> Result<()> {
    let rel_path = strip_root_prefix(path);

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    write_header_from_file_info(&mut header, file_info, mtime_clamp);
    append_xattrs(tar_builder, &file_info.xattrs, path.as_str())
        .with_context(|| format!("appending xattrs for {}", path))?;

    let tar_dir_path = if rel_path.as_str().is_empty() {
        "./".to_string()
    } else {
        format!("{}/", rel_path)
    };
    tar_builder
        .append_data(&mut header, &tar_dir_path, std::io::empty())
        .with_context(|| format!("appending directory {}", path))?;

    Ok(())
}

/// Write a hardlink entry to the tar archive.
fn write_hardlink_entry<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    path: &Utf8Path,
    link_target: &Utf8Path,
    mtime_clamp: u64,
    file_info: &FileInfo,
) -> Result<()> {
    let rel_path = strip_root_prefix(path);
    let rel_target = strip_root_prefix(link_target);

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Link);
    header.set_size(0);
    write_header_from_file_info(&mut header, file_info, mtime_clamp);
    // Mask out file type bits; it's harmless but it matches what GNU tar
    // and Python's tarfile do as well. Not doing this does though result in
    // libarchive's strmode not showing the file as 'h' which shows up in diffs
    // pre vs post-chunkah.
    header.set_mode(file_info.mode & 0o7777);

    tar_builder
        .append_link(&mut header, rel_path.as_str(), rel_target.as_str())
        .with_context(|| format!("appending hardlink {} -> {}", path, link_target))?;

    Ok(())
}

/// Write a regular file entry to the tar archive.
fn write_file_entry<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    rootfs: &cap_std::fs::Dir,
    path: &Utf8Path,
    mtime_clamp: u64,
    file_info: &FileInfo,
) -> Result<()> {
    let rel_path = strip_root_prefix(path);

    let content = rootfs
        .read(rel_path)
        .with_context(|| format!("reading {}", path))?;

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(content.len() as u64);
    write_header_from_file_info(&mut header, file_info, mtime_clamp);
    append_xattrs(tar_builder, &file_info.xattrs, path.as_str())
        .with_context(|| format!("appending xattrs for {}", path))?;

    tar_builder
        .append_data(&mut header, rel_path.as_str(), content.as_slice())
        .with_context(|| format!("appending file {}", path))?;

    Ok(())
}

/// Write a symlink entry to the tar archive.
fn write_symlink_entry<W: Write>(
    tar_builder: &mut tar::Builder<W>,
    rootfs: &cap_std::fs::Dir,
    path: &Utf8Path,
    mtime_clamp: u64,
    file_info: &FileInfo,
) -> Result<()> {
    let rel_path = strip_root_prefix(path);

    let target = rootfs
        .read_link_contents(rel_path)
        .with_context(|| format!("reading symlink {}", path))?;

    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    write_header_from_file_info(&mut header, file_info, mtime_clamp);
    append_xattrs(tar_builder, &file_info.xattrs, path.as_str())
        .with_context(|| format!("appending xattrs for {}", path))?;

    tar_builder
        .append_link(&mut header, rel_path.as_str(), target)
        .with_context(|| format!("appending symlink {}", path))?;

    Ok(())
}

fn write_oci_archive_to<W: Write>(oci_dir: &cap_std::fs::Dir, writer: W) -> Result<()> {
    use cap_std_ext::dirext::CapStdExtDirExt;
    use std::ops::ControlFlow;

    // Template headers for directories and files - cloned and modified as needed
    let dir_header = {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header
    };
    let file_header = {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header
    };

    let mut tar = tar::Builder::new(writer);
    let config = cap_std_ext::dirext::WalkConfiguration::default().sort_by_file_name();

    oci_dir
        .walk(&config, |component| {
            let path = component.path;
            if component.file_type.is_dir() {
                let mut header = dir_header.clone();
                // Tar directories need a trailing slash
                let path_str = format!("{}/", path.display());
                tar.append_data(&mut header, &path_str, std::io::empty())
                    .with_context(|| format!("appending directory {}", path.display()))?;
            } else if component.file_type.is_file() {
                let content = component
                    .dir
                    .read(component.filename)
                    .with_context(|| format!("reading {}", path.display()))?;
                let mut header = file_header.clone();
                header.set_size(content.len() as u64);
                tar.append_data(&mut header, path, content.as_slice())
                    .with_context(|| format!("appending {}", path.display()))?;
            } else {
                anyhow::bail!("unsupported file type for {}", path.display());
            }
            Ok::<_, anyhow::Error>(ControlFlow::Continue(()))
        })
        .context("walking OCI directory")?;

    tar.finish().context("finishing tar archive")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_std::ambient_authority;
    use cap_std::fs::Dir;
    use cap_std_ext::dirext::CapStdExtDirExt;

    /// Helper to create a rootfs in a tempdir, run setup, write files to tar, and return raw bytes.
    /// Optionally accepts a closure to modify the scanned files before writing to tar.
    fn write_tar_bytes<F, M>(setup: F, modify_files: Option<M>, mtime_clamp: u64) -> Vec<u8>
    where
        F: FnOnce(&Dir),
        M: FnOnce(&mut FileMap),
    {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();
        setup(&rootfs);

        let mut files = crate::scan::scan_rootfs(&rootfs, false).unwrap();
        if let Some(modify) = modify_files {
            modify(&mut files);
        }
        let mut output = Vec::new();
        {
            let mut tar_builder = tar::Builder::new(&mut output);
            write_files_to_tar(&mut tar_builder, &rootfs, &files, mtime_clamp).unwrap();
            tar_builder.finish().unwrap();
        }
        output
    }

    /// Helper to create a minimal OCI directory structure.
    fn create_minimal_oci_dir() -> (tempfile::TempDir, Dir) {
        let tmp = tempfile::tempdir().unwrap();
        let oci_dir = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();
        oci_dir
            .write("oci-layout", r#"{"imageLayoutVersion":"1.0.0"}"#)
            .unwrap();
        oci_dir.create_dir("blobs").unwrap();
        (tmp, oci_dir)
    }

    #[test]
    fn test_write_files_to_tar_preserves_xattrs() {
        let output = write_tar_bytes(
            |rootfs| {
                rootfs.write("file", "content").unwrap();
                rootfs
                    .setxattr("file", "user.testattr", b"testvalue")
                    .unwrap();
            },
            None::<fn(&mut FileMap)>,
            1000,
        );

        // Extract using tar command (which properly restores xattrs)
        let extract_dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("tar")
            .args(["xf", "-", "--xattrs"])
            .current_dir(extract_dir.path())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&output).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "tar extraction failed");

        let extracted = Dir::open_ambient_dir(extract_dir.path(), ambient_authority()).unwrap();
        let xattr_value = extracted.getxattr("file", "user.testattr").unwrap();
        assert_eq!(
            xattr_value.as_deref(),
            Some(b"testvalue".as_slice()),
            "file xattr should be preserved after extraction"
        );
    }

    #[test]
    fn test_write_files_to_tar_symlink() {
        let output = write_tar_bytes(
            |rootfs| {
                rootfs.write("target", "content").unwrap();
                rootfs.symlink("target", "link").unwrap();
            },
            None::<fn(&mut FileMap)>,
            1000,
        );

        let mut archive = tar::Archive::new(output.as_slice());
        let mut found_link = false;
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            if entry.header().entry_type() == tar::EntryType::Symlink {
                let link_name = entry.header().link_name().unwrap().unwrap();
                assert_eq!(link_name.to_string_lossy(), "target");
                found_link = true;
            }
        }
        assert!(found_link, "symlink should be in tar");
    }

    #[test]
    fn test_write_files_to_tar_creates_parent_dirs() {
        // Parent directories not in files are created via symlink_metadata() fallback
        let output = write_tar_bytes(
            |rootfs| {
                rootfs.create_dir_all("a/b/c").unwrap();
                rootfs.write("a/b/c/file", "content").unwrap();
            },
            Some(|files: &mut FileMap| {
                // Remove parent dirs to test symlink_metadata() fallback
                files.remove(&Utf8PathBuf::from("/a"));
                files.remove(&Utf8PathBuf::from("/a/b"));
            }),
            1000,
        );

        let mut archive = tar::Archive::new(output.as_slice());
        let paths: Vec<_> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect();

        // Should have all directories and file
        assert!(paths.contains(&"a/".to_string()), "missing a/: {:?}", paths);
        assert!(
            paths.contains(&"a/b/".to_string()),
            "missing a/b/: {:?}",
            paths
        );
        assert!(
            paths.contains(&"a/b/c/".to_string()),
            "missing a/b/c/: {:?}",
            paths
        );
        assert!(
            paths.contains(&"a/b/c/file".to_string()),
            "missing a/b/c/file: {:?}",
            paths
        );

        // Directories should come before the file (sorted order)
        let a_pos = paths.iter().position(|p| p == "a/").unwrap();
        let ab_pos = paths.iter().position(|p| p == "a/b/").unwrap();
        let abc_pos = paths.iter().position(|p| p == "a/b/c/").unwrap();
        let file_pos = paths.iter().position(|p| p == "a/b/c/file").unwrap();
        assert!(a_pos < ab_pos, "a/ should come before a/b/");
        assert!(ab_pos < abc_pos, "a/b/ should come before a/b/c/");
        assert!(abc_pos < file_pos, "a/b/c/ should come before a/b/c/file");
    }

    #[test]
    fn test_write_oci_archive_uncompressed() {
        let (_tmp, oci_dir) = create_minimal_oci_dir();

        let mut output = Vec::new();
        write_oci_archive(&oci_dir, &mut output, ArchiveCompression::None).unwrap();

        // Verify it's a valid tar
        let mut archive = tar::Archive::new(output.as_slice());
        let entries: Vec<_> = archive.entries().unwrap().map(|e| e.unwrap()).collect();
        assert!(!entries.is_empty());
    }

    #[test]
    fn test_write_oci_archive_gzip() {
        let (_tmp, oci_dir) = create_minimal_oci_dir();

        let mut output = Vec::new();
        write_oci_archive(
            &oci_dir,
            &mut output,
            ArchiveCompression::Gzip(flate2::Compression::fast()),
        )
        .unwrap();

        // Verify it's gzip compressed (magic bytes)
        assert_eq!(output[0], 0x1f);
        assert_eq!(output[1], 0x8b);

        // Decompress and verify it's a valid tar
        let decoder = flate2::read::GzDecoder::new(output.as_slice());
        let mut archive = tar::Archive::new(decoder);
        let entries: Vec<_> = archive.entries().unwrap().map(|e| e.unwrap()).collect();
        assert!(!entries.is_empty());
    }

    #[test]
    fn test_write_files_to_tar_hardlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

        // Create a file and a hardlink to it
        rootfs.write("file1", "content").unwrap();
        std::fs::hard_link(tmp.path().join("file1"), tmp.path().join("file2")).unwrap();

        // Create a symlink and a hardlink to it
        rootfs.write("target", "symlink target").unwrap();
        rootfs.symlink("target", "symlink1").unwrap();
        std::fs::hard_link(tmp.path().join("symlink1"), tmp.path().join("symlink2")).unwrap();

        let files = crate::scan::scan_rootfs(&rootfs, false).unwrap();

        // Verify the scan picked up the hardlink metadata for files
        let file1_info = files.get(&Utf8PathBuf::from("/file1")).unwrap();
        let file2_info = files.get(&Utf8PathBuf::from("/file2")).unwrap();
        assert_eq!(file1_info.ino, file2_info.ino, "file inodes should match");
        assert!(file1_info.nlink > 1, "file1 nlink should be > 1");

        // Verify the scan picked up the hardlink metadata for symlinks
        let symlink1_info = files.get(&Utf8PathBuf::from("/symlink1")).unwrap();
        let symlink2_info = files.get(&Utf8PathBuf::from("/symlink2")).unwrap();
        assert_eq!(
            symlink1_info.ino, symlink2_info.ino,
            "symlink inodes should match"
        );
        assert!(symlink1_info.nlink > 1, "symlink1 nlink should be > 1");

        let mut output = Vec::new();
        {
            let mut tar_builder = tar::Builder::new(&mut output);
            write_files_to_tar(&mut tar_builder, &rootfs, &files, 1000).unwrap();
            tar_builder.finish().unwrap();
        }

        // Parse the tar and verify entries
        let mut archive = tar::Archive::new(output.as_slice());
        let mut found_file_hardlink = false;
        let mut found_symlink_hardlink = false;

        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();

            if entry.header().entry_type() == tar::EntryType::Link {
                let link_target = entry
                    .header()
                    .link_name()
                    .unwrap()
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                // file1 comes before file2 alphabetically, so file2 is the hardlink
                if path == "file2" {
                    assert_eq!(link_target, "file1");
                    found_file_hardlink = true;
                }
                // symlink1 comes before symlink2 alphabetically, so symlink2 is the hardlink
                if path == "symlink2" {
                    assert_eq!(link_target, "symlink1");
                    found_symlink_hardlink = true;
                }
            }
        }

        assert!(
            found_file_hardlink,
            "should have a hardlink entry for file2"
        );
        assert!(
            found_symlink_hardlink,
            "should have a hardlink entry for symlink2"
        );

        // Sanity-check they extract as hardlinks
        let extract_dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("tar")
            .args(["xf", "-"])
            .current_dir(extract_dir.path())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&output).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "tar extraction failed");

        use std::os::unix::fs::MetadataExt;

        // Check file hardlinks
        let file1_meta = std::fs::metadata(extract_dir.path().join("file1")).unwrap();
        let file2_meta = std::fs::metadata(extract_dir.path().join("file2")).unwrap();
        assert_eq!(
            file1_meta.ino(),
            file2_meta.ino(),
            "extracted files should have same inode"
        );

        // Check symlink hardlinks
        let symlink1_meta = std::fs::symlink_metadata(extract_dir.path().join("symlink1")).unwrap();
        let symlink2_meta = std::fs::symlink_metadata(extract_dir.path().join("symlink2")).unwrap();
        assert_eq!(
            symlink1_meta.ino(),
            symlink2_meta.ino(),
            "extracted symlinks should have same inode"
        );
    }
}
