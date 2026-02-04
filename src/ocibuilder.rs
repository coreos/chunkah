use std::collections::HashMap;
use std::io::Write;

use anyhow::{Context, Result};
use cap_std_ext::cap_std::fs::Dir;
use ocidir::oci_spec::image as oci_image;

use crate::components::Component;

/// Compression settings for the OCI image.
#[derive(Clone, Copy, Default)]
pub enum Compression {
    /// No compression.
    #[default]
    None,
    /// Gzip compression with the specified level (0-9).
    Gzip(u32),
}

/// Builder for creating OCI images from components.
pub struct Builder {
    /// The rootfs to build from.
    rootfs: Dir,
    /// The OCI directory to build into.
    oci_dir: cap_std_ext::cap_tempfile::TempDir,
    /// The components to include in the image, ordered by stability descending.
    components: Vec<(String, Component)>,
    /// Compression settings for layers and archive.
    compression: Compression,
    /// Annotations to add to the image manifest.
    annotations: Option<HashMap<String, String>>,
    /// The image configuration.
    config: Option<oci_image::ImageConfiguration>,
}

impl Builder {
    /// Create a new Builder with required parameters.
    pub fn new(rootfs: &Dir, components: Vec<(String, Component)>) -> Result<Self> {
        let oci_dir = cap_std_ext::cap_tempfile::tempdir(cap_std_ext::cap_std::ambient_authority())
            .context("creating temp directory")?;

        Ok(Self {
            rootfs: rootfs.try_clone().context("cloning rootfs")?,
            oci_dir,
            components,
            compression: Compression::default(),
            annotations: None,
            config: None,
        })
    }

    /// Set the compression settings.
    pub fn compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Set annotations to add to the image manifest.
    pub fn annotations(mut self, annotations: HashMap<String, String>) -> Self {
        self.annotations = Some(annotations);
        self
    }

    /// Set the image configuration.
    pub fn config(mut self, config: oci_image::ImageConfiguration) -> Self {
        self.config = Some(config);
        self
    }

    /// Build the OCI image and write it to the given output.
    pub fn build<W: Write>(self, output: &mut W) -> Result<()> {
        self.build_oci_dir().context("building OCI directory")?;

        let compressed = matches!(self.compression, Compression::Gzip(_));
        tracing::info!(compressed = compressed, "writing OCI archive");

        let compression = match self.compression {
            Compression::None => crate::tar::ArchiveCompression::None,
            Compression::Gzip(level) => {
                crate::tar::ArchiveCompression::Gzip(flate2::Compression::new(level))
            }
        };

        crate::tar::write_oci_archive(&self.oci_dir, &mut *output, compression)
            .context("writing OCI archive")?;

        output.flush().context("flushing output")
    }

    fn build_oci_dir(&self) -> Result<()> {
        let oci_dir =
            ocidir::OciDir::ensure(self.oci_dir.try_clone().context("cloning temp directory")?)
                .context("creating OCI directory")?;

        // first, let's create an empty manifest
        let mut manifest = oci_dir
            .new_empty_manifest()
            .context("creating empty manifest")?
            .build()
            .context("building manifest")?;

        let mut config = self.config.clone().unwrap_or_default();

        // this is the important bit: we add all the layers
        self.add_components(&mut manifest, &mut config)
            .context("adding layers to OCI directory")?;

        if let Some(annotations) = &self.annotations {
            manifest.set_annotations(Some(annotations.clone()));
        }

        let arch = config.architecture().to_string();
        let platform = oci_image::PlatformBuilder::default()
            .os("linux")
            .architecture(arch.as_str())
            .build()
            .context("building platform")?;

        oci_dir
            .insert_manifest_and_config(manifest, config, None, platform)
            .context("inserting manifest and config")?;

        Ok(())
    }

    /// Add layers to the OCI directory and update the manifest and config.
    fn add_components(
        &self,
        manifest: &mut oci_image::ImageManifest,
        config: &mut oci_image::ImageConfiguration,
    ) -> Result<()> {
        for (name, component) in &self.components {
            if component.files.is_empty() {
                tracing::debug!(component = %name, "skipping empty component");
                continue;
            }
            self.add_component(manifest, config, name, component)
                .with_context(|| format!("adding component {}", name))?;
        }

        Ok(())
    }

    /// Add a single component as a layer to the OCI directory.
    fn add_component(
        &self,
        manifest: &mut oci_image::ImageManifest,
        config: &mut oci_image::ImageConfiguration,
        name: &str,
        component: &Component,
    ) -> Result<()> {
        let oci_dir = ocidir::OciDir::open(self.oci_dir.try_clone().context("cloning oci_dir")?)
            .context("opening OCI directory")?;
        tracing::debug!(components = name, "creating tar layer");
        let mut tar_builder =
            crate::tar::create_layer(&oci_dir, self.compression).context("creating layer")?;

        crate::tar::write_files_to_tar(
            &mut tar_builder,
            &self.rootfs,
            &component.files,
            component.mtime_clamp,
        )
        .context("building tar layer")?;

        tar_builder.finish().context("finishing layer tar")?;
        let layer = tar_builder
            .into_inner()
            .context("getting layer writer")?
            .complete()
            .context("completing layer")?;

        let annotations = {
            let mut hm = HashMap::new();
            hm.insert("org.chunkah.component".to_string(), name.to_string());
            hm.insert(
                "org.chunkah.stability".to_string(),
                format!("{:.3}", component.stability),
            );
            hm
        };

        let mtime_i64 =
            i64::try_from(component.mtime_clamp).context("mtime_clamp overflows i64")?;

        let created = chrono::DateTime::from_timestamp(mtime_i64, 0)
            .with_context(|| format!("invalid mtime_clamp: {}", component.mtime_clamp))?
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let history = oci_image::HistoryBuilder::default()
            .created(created)
            .author("chunkah".to_string())
            .created_by("chunkah".to_string())
            .comment(name.to_string())
            .build()
            .context("building history entry")?;

        oci_dir.push_layer_with_history_annotated(
            manifest,
            config,
            layer,
            Some(annotations),
            Some(history),
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use cap_std_ext::cap_std::ambient_authority;
    use cap_std_ext::cap_std::fs::PermissionsExt;
    use cap_std_ext::dirext::CapStdExtDirExt;
    use maplit::btreeset;
    use std::collections::BTreeSet;

    use crate::components::FileMap;

    /// Helper struct for test results
    struct TestOciResult {
        oci_dir: ocidir::OciDir,
        manifest: oci_image::ImageManifest,
        image_config: oci_image::ImageConfiguration,
        /// Keep tempdir alive for the lifetime of the test
        _oci_tempdir: tempfile::TempDir,
    }

    impl TestOciResult {
        fn first_layer(&self) -> &oci_image::Descriptor {
            &self.manifest.layers()[0]
        }

        fn read_layer_tar(
            &self,
            layer: &oci_image::Descriptor,
        ) -> tar::Archive<impl std::io::Read + '_> {
            tar::Archive::new(self.oci_dir.read_blob(layer).unwrap())
        }

        fn get_layer_tar_entries(
            &self,
            layer: &oci_image::Descriptor,
        ) -> Vec<(String, tar::EntryType, u64)> {
            let mut layer_tar = self.read_layer_tar(layer);
            layer_tar
                .entries()
                .unwrap()
                .map(|e| {
                    let entry = e.unwrap();
                    let path = entry.path().unwrap().to_string_lossy().to_string();
                    let entry_type = entry.header().entry_type();
                    let size = entry.header().size().unwrap();
                    (path, entry_type, size)
                })
                .collect()
        }
    }

    /// Specification for a test component: (name, paths, mtime_clamp)
    type ComponentSpec = (&'static str, BTreeSet<Utf8PathBuf>, u64);

    /// Helper to build an OCI archive and extract it for inspection.
    fn build_and_extract<F>(rootfs_setup: F, specs: Vec<ComponentSpec>) -> TestOciResult
    where
        F: FnOnce(&Dir),
    {
        // Create temp rootfs and run setup
        let rootfs_dir = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(rootfs_dir.path(), ambient_authority()).unwrap();
        rootfs_setup(&rootfs);

        // Scan the rootfs once
        let all_files = crate::scan::Scanner::new(&rootfs).scan().unwrap();

        // Build components by selecting paths from scanned files
        let components: Vec<(String, Component)> = specs
            .into_iter()
            .map(|(name, paths, mtime_clamp)| {
                let files: FileMap = paths
                    .iter()
                    .filter_map(|p| all_files.get(p).map(|info| (p.clone(), info.clone())))
                    .collect();
                (
                    name.to_string(),
                    Component {
                        mtime_clamp,
                        stability: 0.0,
                        files,
                    },
                )
            })
            .collect();

        // Create minimal config
        let config = oci_image::ImageConfigurationBuilder::default()
            .os("linux")
            .architecture("amd64")
            .rootfs(
                oci_image::RootFsBuilder::default()
                    .typ("layers")
                    .diff_ids(Vec::<String>::new())
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        // Build OCI archive
        let builder = Builder::new(&rootfs, components)
            .unwrap()
            .compression(Compression::None)
            .config(config);
        let mut output = Vec::new();
        builder.build(&mut output).unwrap();

        // Extract to tempdir
        let oci_tempdir = tempfile::tempdir().unwrap();
        let mut archive = tar::Archive::new(output.as_slice());
        archive.unpack(oci_tempdir.path()).unwrap();

        // Open with ocidir
        let oci_dir_cap = Dir::open_ambient_dir(oci_tempdir.path(), ambient_authority()).unwrap();
        let oci_dir = ocidir::OciDir::open(oci_dir_cap).unwrap();

        // Get manifest
        let index = oci_dir.read_index().unwrap();
        let manifest_desc = index.manifests().first().unwrap();
        let manifest: oci_image::ImageManifest = oci_dir.read_json_blob(manifest_desc).unwrap();

        // Get image config
        let image_config: oci_image::ImageConfiguration =
            oci_dir.read_json_blob(manifest.config()).unwrap();

        TestOciResult {
            oci_dir,
            manifest,
            image_config,
            _oci_tempdir: oci_tempdir,
        }
    }

    #[test]
    fn test_components_to_layers() {
        let result = build_and_extract(
            |rootfs| {
                rootfs.write("file_a", "content a").unwrap();
                rootfs.write("file_b", "content b").unwrap();
            },
            vec![
                (
                    "component_a",
                    btreeset! { Utf8PathBuf::from("/file_a") },
                    1000,
                ),
                (
                    "component_b",
                    btreeset! { Utf8PathBuf::from("/file_b") },
                    1000,
                ),
            ],
        );

        assert_eq!(result.manifest.layers().len(), 2);

        // Collect layer info (order is not guaranteed with HashMap)
        let layer_info: Vec<_> = result
            .manifest
            .layers()
            .iter()
            .map(|layer| {
                let component = layer
                    .annotations()
                    .as_ref()
                    .and_then(|a| a.get("org.chunkah.component"))
                    .cloned();
                let entries = result.get_layer_tar_entries(layer);
                (component, entries)
            })
            .collect();

        // Find component_a layer
        let layer_a = layer_info
            .iter()
            .find(|(c, _)| c.as_deref() == Some("component_a"))
            .expect("should have component_a layer");
        assert_eq!(layer_a.1.len(), 1);
        assert_eq!(layer_a.1[0].0, "file_a");
        assert_eq!(layer_a.1[0].1, tar::EntryType::Regular);
        assert_eq!(layer_a.1[0].2, 9); // "content a".len()

        // Find component_b layer
        let layer_b = layer_info
            .iter()
            .find(|(c, _)| c.as_deref() == Some("component_b"))
            .expect("should have component_b layer");
        assert_eq!(layer_b.1.len(), 1);
        assert_eq!(layer_b.1[0].0, "file_b");
        assert_eq!(layer_b.1[0].1, tar::EntryType::Regular);
        assert_eq!(layer_b.1[0].2, 9); // "content b".len()

        // Verify history entries match layers
        let history = result
            .image_config
            .history()
            .as_ref()
            .expect("image should have history entries");
        assert_eq!(
            history.len(),
            result.manifest.layers().len(),
            "history entries should match layer count"
        );

        // Check each history entry has expected fields
        for entry in history {
            assert_eq!(
                entry.author().as_deref(),
                Some("chunkah"),
                "history author should be 'chunkah'"
            );
            assert_eq!(
                entry.created_by().as_deref(),
                Some("chunkah"),
                "history created_by should be 'chunkah'"
            );
            assert!(
                entry.comment().is_some(),
                "history should have comment with component name"
            );
            assert!(
                entry.created().is_some(),
                "history should have created timestamp"
            );
        }

        // Verify component names in history comments
        let comments: Vec<_> = history.iter().filter_map(|h| h.comment().clone()).collect();
        assert!(
            comments.iter().any(|c| c == "component_a"),
            "history comments should contain component_a"
        );
        assert!(
            comments.iter().any(|c| c == "component_b"),
            "history comments should contain component_b"
        );
    }

    #[test]
    fn test_file_metadata() {
        let result = build_and_extract(
            |rootfs| {
                rootfs.write("executable", "#!/bin/sh\necho hi").unwrap();
                // Make it executable (0o755)
                let meta = rootfs.metadata("executable").unwrap();
                let mut perms = meta.permissions();
                perms.set_mode(0o755);
                rootfs.set_permissions("executable", perms).unwrap();

                rootfs.create_dir("mydir").unwrap();
                rootfs
                    .set_permissions(
                        "mydir",
                        cap_std_ext::cap_std::fs::Permissions::from_mode(0o777),
                    )
                    .unwrap();

                rootfs.write("xattr_file", "xattr content").unwrap();
                rootfs
                    .setxattr("xattr_file", "user.myattr", b"myvalue")
                    .unwrap();

                rootfs.write("target", "target content").unwrap();
                rootfs.symlink("target", "link").unwrap();
            },
            vec![(
                "test",
                btreeset! {
                    Utf8PathBuf::from("/executable"),
                    Utf8PathBuf::from("/link"),
                    Utf8PathBuf::from("/mydir"),
                    Utf8PathBuf::from("/target"),
                    Utf8PathBuf::from("/xattr_file"),
                },
                1000,
            )],
        );

        // Get the layer and check metadata
        let mut layer_tar = result.read_layer_tar(result.first_layer());
        let mut found_xattr = false;
        for entry in layer_tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            let header = entry.header();

            // Check mtime is clamped
            let mtime = header.mtime().unwrap();
            assert!(mtime <= 1000, "mtime should be clamped: {}", mtime);

            if path == "executable" {
                // Check mode preserves executable bit
                let mode = header.mode().unwrap();
                assert!(
                    mode & 0o111 != 0,
                    "executable should have execute bits: {:o}",
                    mode
                );
            } else if path == "mydir/" {
                assert_eq!(header.entry_type(), tar::EntryType::Directory);
                // Verify atypical 777 permissions are preserved
                let mode = header.mode().unwrap();
                assert_eq!(
                    mode & 0o777,
                    0o777,
                    "directory should have 777 permissions, got {:o}",
                    mode & 0o777
                );
            } else if path == "xattr_file" {
                // Check xattr survives
                let pax = entry.pax_extensions().unwrap().unwrap();
                for ext in pax {
                    let ext = ext.unwrap();
                    if ext.key().unwrap() == "SCHILY.xattr.user.myattr" {
                        assert_eq!(ext.value_bytes(), b"myvalue");
                        found_xattr = true;
                    }
                }
                assert!(found_xattr, "xattr should be preserved in layer");
            } else if path == "link" {
                // Check symlink
                assert_eq!(header.entry_type(), tar::EntryType::Symlink);
                let link_target = header.link_name().unwrap().unwrap();
                assert_eq!(link_target.to_string_lossy(), "target");
            }
        }
    }

    #[test]
    fn test_nested_directory_structure() {
        let result = build_and_extract(
            |rootfs| {
                rootfs.create_dir_all("usr/bin").unwrap();
                rootfs.write("usr/bin/myapp", "app content").unwrap();
            },
            vec![(
                "test",
                btreeset! {
                    Utf8PathBuf::from("/usr"),
                    Utf8PathBuf::from("/usr/bin"),
                    Utf8PathBuf::from("/usr/bin/myapp"),
                },
                1000,
            )],
        );

        // Layer should contain the file and parent directories
        let entries = result.get_layer_tar_entries(&result.manifest.layers()[0]);

        // Should have /usr, /usr/bin, and /usr/bin/myapp
        // Note: directories have trailing slashes in tar
        let paths: Vec<_> = entries.iter().map(|(p, _, _)| p.as_str()).collect();
        assert!(paths.contains(&"usr/"), "should contain usr/: {:?}", paths);
        assert!(
            paths.contains(&"usr/bin/"),
            "should contain usr/bin/: {:?}",
            paths
        );
        assert!(
            paths.contains(&"usr/bin/myapp"),
            "should contain usr/bin/myapp: {:?}",
            paths
        );
    }

    #[test]
    fn test_mtime_clamping() {
        use fs_set_times::{SetTimes, SystemTimeSpec};

        let result = build_and_extract(
            |rootfs| {
                // This file has current mtime (>> 500), should be clamped to 500
                rootfs.write("clamped_file", "content").unwrap();

                // This file has mtime 200 (< 500), should stay at 200
                rootfs.write("unclamped_file", "content").unwrap();
                let file = rootfs.open("unclamped_file").unwrap();
                let mtime = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(200);
                file.set_times(None, Some(SystemTimeSpec::Absolute(mtime)))
                    .unwrap();
            },
            vec![(
                "test",
                btreeset! {
                    Utf8PathBuf::from("/clamped_file"),
                    Utf8PathBuf::from("/unclamped_file"),
                },
                500,
            )],
        );

        let mut layer_tar = result.read_layer_tar(result.first_layer());
        for entry in layer_tar.entries().unwrap() {
            let entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().to_string();
            let mtime = entry.header().mtime().unwrap();
            if path == "clamped_file" {
                assert_eq!(mtime, 500, "clamped_file mtime should be clamped to 500");
            } else if path == "unclamped_file" {
                assert_eq!(mtime, 200, "unclamped_file mtime should stay at 200");
            }
        }
    }
}
