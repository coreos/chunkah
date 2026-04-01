use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use cap_std_ext::cap_std::ambient_authority;
use cap_std_ext::cap_std::fs::Dir;
use clap::Parser;
use ocidir::oci_spec::image as oci_image;
use serde::{Deserialize, Serialize};

use crate::components::{Component, ComponentsRepos, FileMap};
use crate::ocibuilder::{Builder, Compression};
use crate::packing::{PackItem, calculate_packing};
use crate::utils;

#[derive(Parser, Default)]
pub struct BuildArgs {
    /// Path to the rootfs to build from
    #[arg(long, env = "CHUNKAH_ROOTFS", hide_env_values = true)]
    rootfs: Utf8PathBuf,

    /// Output file path (defaults to stdout)
    #[arg(short, long, value_name = "PATH")]
    output: Option<Utf8PathBuf>,

    /// Maximum number of layers to output
    #[arg(long, default_value_t = 64)]
    max_layers: usize,

    /// Read image config from a JSON file
    ///
    /// The file should contain the .Config element from a podman/docker
    /// inspect output. This is useful when resplitting an existing image.
    #[arg(long = "config", value_name = "PATH", conflicts_with = "config_str")]
    config: Option<Utf8PathBuf>,

    /// Read image config from a JSON string
    ///
    /// Same as --config but takes a JSON string directly instead of a file path.
    #[arg(
        long = "config-str",
        value_name = "JSON",
        env = "CHUNKAH_CONFIG_STR",
        hide_env_values = true
    )]
    config_str: Option<String>,

    /// Add or remove a label from the image
    ///
    /// Format: KEY=VALUE to set, KEY- to remove, or - to clear all.
    /// Operations are processed in order; - clears both base config and prior CLI labels.
    #[arg(long = "label", value_name = "KEY=VALUE|KEY-|-")]
    labels: Vec<String>,

    /// Add an annotation to the image manifest
    ///
    /// Format: KEY=VALUE. Can be specified multiple times.
    #[arg(long = "annotation", value_name = "KEY=VALUE")]
    annotations: Vec<String>,

    /// Unix timestamp used as the creation time for the OCI image and as
    /// the maximum mtime for files without a known build time.
    #[arg(
        long,
        value_name = "EPOCH",
        env = "SOURCE_DATE_EPOCH",
        hide_env_values = true
    )]
    source_date_epoch: Option<u64>,

    /// Compress layers and the OCI archive with gzip
    ///
    /// By default, layers and the OCI archive are uncompressed. This flag
    /// enables gzip compression for both.
    #[arg(long)]
    compressed: bool,

    /// Gzip compression level (0-9, default: 6)
    ///
    /// Level 0 is no compression (fastest), 9 is maximum compression (slowest).
    /// Only applies when --compressed is specified.
    #[arg(long, value_name = "LEVEL", default_value_t = 6, value_parser = clap::value_parser!(u32).range(0..=9))]
    compression_level: u32,

    /// Target architecture for the output image
    ///
    /// If not provided, the architecture from the config is used if found, or
    /// the current system architecture otherwise.
    #[arg(long, value_name = "ARCH")]
    arch: Option<String>,

    /// Skip special files (sockets, FIFOs, block/char devices)
    ///
    /// By default, chunkah fails when encountering special file types.
    /// This flag causes them to be silently skipped instead.
    #[arg(long)]
    skip_special_files: bool,

    /// Paths to exclude from the rootfs
    ///
    /// If a directory ends with `/`, its contents are excluded but not the
    /// directory itself. Can be specified multiple times. Paths must be
    /// absolute.
    #[arg(long = "prune", value_name = "PATH")]
    prune: Vec<Utf8PathBuf>,

    /// Number of threads for parallel layer writing (0 = auto-detect)
    #[arg(short = 'T', long, default_value_t = 0, env = "CHUNKAH_THREADS")]
    threads: usize,

    /// Write peak memory usage (in bytes) to a file
    #[arg(long, value_name = "PATH", hide = true)]
    write_peak_mem_to: Option<Utf8PathBuf>,

    /// Write a component manifest JSON to a file
    #[arg(long, value_name = "PATH", hide = true)]
    write_manifest_to: Option<Utf8PathBuf>,
}

impl BuildArgs {
    /// Apply CLI overrides to an OCI config, returning a new config.
    fn apply_to_config(&self, config: oci_image::Config) -> Result<oci_image::Config> {
        let mut builder = oci_image::ConfigBuilder::default();

        // Copy over all fields from base config. Would be nice if we could
        // instantiate a ConfigBuilder from a starting Config instead...
        macro_rules! copy_if_present {
            ($($field:ident),+) => {
                $(
                    if let Some(v) = config.$field() {
                        builder = builder.$field(v.clone());
                    }
                )+
            };
        }
        copy_if_present!(
            user,
            working_dir,
            stop_signal,
            entrypoint,
            cmd,
            env,
            exposed_ports,
            volumes
        );

        // labels; CLI args override config
        let labels =
            parse_key_value_pairs(&self.labels, config.labels().clone().unwrap_or_default())
                .context("parsing labels")?;
        if !labels.is_empty() {
            builder = builder.labels(labels);
        }

        builder.build().context("building config")
    }
}

pub fn run(args: &BuildArgs) -> Result<()> {
    tracing::info!(rootfs = %args.rootfs, "starting build");

    const CONTAINERS_STORAGE_LAYER_LIMIT: usize = 500;
    if args.max_layers > CONTAINERS_STORAGE_LAYER_LIMIT {
        tracing::warn!(
            max_layers = args.max_layers,
            limit = CONTAINERS_STORAGE_LAYER_LIMIT,
            "image exceeds known containers-storage layer limit"
        );
    }

    // load base config from file, string, or use empty default
    let parsed = if let Some(path) = &args.config {
        tracing::debug!(source = %path, "loading config from file");
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path))?;
        parse_config(&content).with_context(|| format!("failed to parse config file: {}", path))?
    } else if let Some(config_str) = &args.config_str {
        tracing::debug!("loading config from string");
        parse_config(config_str).context("failed to parse config string")?
    } else {
        tracing::debug!("using default config");
        ParsedConfig {
            config: oci_image::Config::default(),
            annotations: HashMap::new(),
            architecture: None,
            created: None,
        }
    };

    let created_epoch = resolve_created_epoch(args.source_date_epoch, &parsed)?;

    let architecture = args.arch.as_deref().or(parsed.architecture.as_deref());
    // get the current arch if not provided, but even if provided, this
    // normalizes the arch so that `--arch x86_64` also works
    let architecture = utils::get_goarch(architecture);
    tracing::debug!(architecture = architecture, "target architecture");

    // merge config and CLI annotations
    let annotations = parse_key_value_pairs(&args.annotations, parsed.annotations)
        .context("parsing annotations")?;

    let image_config = build_image_config(args, parsed.config, created_epoch, architecture)
        .context("building image config")?;

    let rootfs = Dir::open_ambient_dir(args.rootfs.as_std_path(), ambient_authority())
        .with_context(|| format!("opening rootfs {}", args.rootfs))?;

    let files = crate::scan::Scanner::new(&rootfs)
        .skip_special_files(args.skip_special_files)
        .prune(&args.prune)?
        .scan()
        .with_context(|| format!("scanning {} for files", args.rootfs))?;
    let total_size: u64 = files.values().map(|f| f.size).sum();
    tracing::info!(files = files.len(), size = %utils::format_size(total_size), "scan complete");

    warn_ostree_sysroot(&files);

    let repos =
        ComponentsRepos::load(&rootfs, &files, created_epoch).context("loading components")?;
    if repos.is_empty() {
        anyhow::bail!("no supported component repo found in rootfs");
    }

    let components = repos
        .into_components(&rootfs, files)
        .context("assigning components")?;
    tracing::info!(components = components.len(), "components assigned");

    // write the component manifest before packing merges components
    if let Some(path) = &args.write_manifest_to {
        let file = std::fs::File::create(path)
            .with_context(|| format!("creating manifest file {path}"))?;
        write_manifest(&components, file).with_context(|| format!("writing manifest to {path}"))?;
    }

    // pack components down to max layers
    let components = pack_components(args.max_layers, components).context("packing components")?;
    tracing::info!(layers = components.len(), "packing complete");

    // build the OCI image
    let compression = if args.compressed {
        Compression::Gzip(args.compression_level)
    } else {
        Compression::None
    };

    let threads = NonZeroUsize::new(args.threads).unwrap_or_else(|| {
        match std::thread::available_parallelism() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(err = %e, "failed to detect available parallelism, defaulting to 1");
                NonZeroUsize::MIN
            }
        }
    });

    let builder = Builder::new(&rootfs, components)
        .context("creating builder")?
        .compression(compression)
        .threads(threads)
        .annotations(annotations)
        .config(image_config);

    if let Some(output_path) = &args.output {
        tracing::info!(output = %output_path, "writing to file");
        let mut file = std::fs::File::create(output_path)
            .with_context(|| format!("creating output file {}", output_path))?;
        builder.build(&mut file)?;
    } else {
        tracing::info!("writing to stdout");
        builder.build(&mut std::io::stdout().lock())?;
    }

    if let Some(path) = &args.write_peak_mem_to {
        let peak_rss = utils::get_peak_rss().context("getting peak memory usage")?;
        std::fs::write(path, format!("{peak_rss}\n"))
            .with_context(|| format!("writing peak memory to {path}"))?;
    }

    tracing::info!("build complete");
    Ok(())
}

/// Check whether the file map contains an OSTree sysroot repo.
/// The scanner always includes parent directories, so the exact path
/// will be present if any children under it were scanned.
fn filemap_has_ostree(files: &FileMap) -> bool {
    files.contains_key(&Utf8PathBuf::from("/sysroot/ostree"))
}

/// Warn if the scanned files include an OSTree sysroot repo. This means the
/// user didn't pass `--prune /sysroot/` and the object store will be chunked,
/// which produces poor results. The warning sleeps briefly so it isn't lost
/// in scrolling output.
fn warn_ostree_sysroot(files: &FileMap) {
    if !filemap_has_ostree(files) {
        return;
    }

    tracing::warn!(
        "rootfs contains sysroot/ostree which was not pruned; \
         chunking an ostree object store produces poor results. \
         Use --prune /sysroot/ to exclude it. \
         See: https://github.com/coreos/chunkah?tab=readme-ov-file#compatibility-with-bootable-bootc-images"
    );
    std::thread::sleep(std::time::Duration::from_secs(7));
}

/// Serialize the component map into a JSON manifest.
fn write_manifest(
    components: &HashMap<String, Component>,
    writer: impl std::io::Write,
) -> Result<()> {
    let manifest = Manifest {
        components: components
            .iter()
            .map(|(name, component)| {
                let entry = ManifestComponent {
                    file_count: component.files.len(),
                    size: component.files.values().map(|f| f.size).sum(),
                    files: component.files.keys().map(|p| p.to_string()).collect(),
                };
                (name.clone(), entry)
            })
            .collect(),
    };
    serde_json::to_writer_pretty(writer, &manifest).context("serializing manifest to JSON")
}

/// Parse config from a JSON string.
///
/// Supports three formats:
/// 1. Direct OCI config (e.g., `{"Entrypoint": [...]}`)
/// 2. podman/docker inspect output array (e.g., `[{"Config": {...}}]`)
/// 3. Single inspect output object (e.g., `{"Config": {...}}`)
fn parse_config(json_str: &str) -> Result<ParsedConfig> {
    let input: ConfigInput =
        serde_json::from_str(json_str).context("failed to parse config JSON")?;
    match input {
        ConfigInput::Inspect(mut vec) => vec
            .pop()
            .ok_or_else(|| anyhow::anyhow!("inspect output is an empty array")),
        ConfigInput::InspectOne(parsed) => Ok(parsed),
        ConfigInput::Direct(config) => Ok(ParsedConfig {
            config,
            annotations: HashMap::new(),
            architecture: None,
            created: None,
        }),
    }
}

/// Parsed config data from either OCI config or podman/docker inspect format.
/// The serde renames allow this to deserialize from inspect format (with "Config" key).
#[derive(Deserialize)]
struct ParsedConfig {
    #[serde(rename = "Config")]
    config: oci_image::Config,
    #[serde(rename = "Annotations", default)]
    annotations: HashMap<String, String>,
    #[serde(rename = "Architecture")]
    architecture: Option<String>,
    #[serde(rename = "Created")]
    created: Option<String>,
}

/// Config input format - either direct OCI config or podman/docker inspect output.
#[derive(Deserialize)]
#[serde(untagged)]
enum ConfigInput {
    /// podman/docker inspect output (array with Config field)
    Inspect(Vec<ParsedConfig>),
    /// Single inspect output object (e.g., first element extracted from array)
    InspectOne(ParsedConfig),
    /// Direct OCI config (e.g., `{"Entrypoint": [...]}`)
    Direct(oci_image::Config),
}

/// Top-level manifest written via --write-manifest-to.
#[derive(Serialize)]
struct Manifest {
    components: BTreeMap<String, ManifestComponent>,
}

/// Per-component entry in the embedded manifest.
#[derive(Serialize)]
struct ManifestComponent {
    file_count: usize,
    size: u64,
    files: Vec<String>,
}

/// Resolve the created epoch from CLI, config, or current time.
///
/// Priority: explicit `--source-date-epoch` > `Created` from image inspect > current time.
fn resolve_created_epoch(source_date_epoch: Option<u64>, parsed: &ParsedConfig) -> Result<u64> {
    if let Some(epoch) = source_date_epoch {
        tracing::debug!(epoch, "using source date epoch from CLI/env");
        Ok(epoch)
    } else if let Some(created) = &parsed.created {
        let epoch =
            utils::parse_rfc3339_epoch(created).context("parsing image created timestamp")?;
        tracing::debug!(epoch, created = %created, "using build date from image config");
        Ok(epoch)
    } else {
        let epoch = utils::get_current_epoch()?;
        tracing::debug!(epoch, "using current time as source date epoch");
        Ok(epoch)
    }
}

/// Build the OCI image configuration from CLI args and a parsed config.
fn build_image_config(
    args: &BuildArgs,
    config: oci_image::Config,
    created: u64,
    architecture: &str,
) -> Result<oci_image::ImageConfiguration> {
    // apply CLI configs to base OCI config
    let config = args
        .apply_to_config(config)
        .context("applying CLI configs")?;

    // this is empty for now; it gets populated as we add components
    let rootfs = oci_image::RootFsBuilder::default()
        .typ("layers")
        .diff_ids(Vec::<String>::new())
        .build()?;

    // format the created timestamp as RFC 3339
    let epoch_i64 = i64::try_from(created)
        .with_context(|| format!("created timestamp overflows i64: {created}"))?;
    let created = chrono::DateTime::from_timestamp(epoch_i64, 0)
        .with_context(|| format!("invalid created timestamp: {}", created))?
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let image_config = oci_image::ImageConfigurationBuilder::default()
        .os("linux")
        .architecture(architecture)
        .config(config)
        .rootfs(rootfs)
        .created(created)
        .build()?;

    Ok(image_config)
}

/// Parse KEY=VALUE pairs and merge into an existing map.
///
/// Supports three formats:
/// - `key=value`: set or override a key
/// - `key-`: remove a key (trailing dash)
/// - `-`: clear all keys
fn parse_key_value_pairs(
    pairs: &[String],
    mut map: HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    for pair in pairs {
        if let Some((k, v)) = pair.split_once('=') {
            anyhow::ensure!(!k.is_empty(), "key cannot be empty: {pair}");
            map.insert(k.to_string(), v.to_string());
        } else if let Some(k) = pair.strip_suffix('-') {
            if k.is_empty() {
                map.clear();
            } else {
                map.remove(k);
            }
        } else {
            anyhow::bail!("label must be in KEY=VALUE or KEY- format: {pair}");
        }
    }
    Ok(map)
}

/// Packs components into layers according to max_layers constraint.
fn pack_components(
    max_layers: usize,
    components: HashMap<String, Component>,
) -> Result<Vec<(String, Component)>> {
    let mut entries: Vec<Option<(String, Component)>> = components.into_iter().map(Some).collect();
    // sort by component name for deterministic inputs to the packing algorithm
    entries.sort_by(|a, b| a.as_ref().unwrap().0.cmp(&b.as_ref().unwrap().0));

    let items: Vec<PackItem> = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let (name, comp) = entry.as_ref().unwrap();
            let size = comp.files.values().map(|f| f.size).sum();
            tracing::trace!(idx = idx, name = %name, size = size, stability = comp.stability, "packing item");
            PackItem {
                size,
                stability: comp.stability,
            }
        })
        .collect();

    let packed_groups = calculate_packing(&items, max_layers);

    let mut result = Vec::with_capacity(packed_groups.len());

    for group in packed_groups {
        if group.indices.len() == 1 {
            // single component group
            let idx = group.indices[0];
            let (name, component) = entries[idx].take().expect("packing returned invalid index");
            result.push((name, component));
        } else {
            // merged group - combine components
            let mut names = Vec::with_capacity(group.indices.len());
            let mut merged_files = FileMap::new();
            let mut max_mtime_clamp = 0u64;

            for &idx in &group.indices {
                let (name, comp) = entries[idx].take().expect("packing returned invalid index");
                names.push(name);
                // Move "up" the clamp. We're still guaranteed that it's (1)
                // a reproducible timestamp for this particular group, and
                // (2) it's very unlikely to be after $now so we'll clamp
                // scriptlet-created files.
                max_mtime_clamp = max_mtime_clamp.max(comp.mtime_clamp);
                merged_files.extend(comp.files);
            }

            // this becomes history/annotation values; sort for reproducibility
            names.sort();
            let merged_name = names.join(" ");
            result.push((
                merged_name,
                Component {
                    mtime_clamp: max_mtime_clamp,
                    stability: group.stability,
                    files: merged_files,
                },
            ));
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG_FIXTURE: &str = include_str!("../tests/fixtures/empty.image-config.json");

    fn scan_tempdir(rootfs: &Dir) -> FileMap {
        crate::scan::Scanner::new(rootfs).scan().unwrap()
    }

    #[test]
    fn test_filemap_has_ostree() {
        use cap_std_ext::cap_tempfile;

        let td = cap_tempfile::tempdir(ambient_authority()).unwrap();
        assert!(!filemap_has_ostree(&scan_tempdir(&td)));

        // Non-ostree paths should not match
        td.create_dir_all("usr/bin").unwrap();
        td.write("usr/bin/ostree", "fake").unwrap();
        td.create_dir_all("sysroot").unwrap();
        td.write("sysroot/config", "fake").unwrap();
        assert!(!filemap_has_ostree(&scan_tempdir(&td)));

        // Adding sysroot/ostree should match
        td.create_dir_all("sysroot/ostree/repo").unwrap();
        td.write("sysroot/ostree/repo/config", "fake").unwrap();
        assert!(filemap_has_ostree(&scan_tempdir(&td)));
    }

    #[test]
    fn test_emptydir_roundtrip() {
        // Create an OCI archive from an empty rootfs. Then re-open it with
        // ocidir and check it's what we expect.

        // create a temp directory for the empty rootfs
        let rootfs_dir = tempfile::tempdir().unwrap();

        let args = BuildArgs {
            rootfs: Utf8PathBuf::try_from(rootfs_dir.path().to_path_buf()).unwrap(),
            source_date_epoch: Some(1),
            ..Default::default()
        };

        // parse config from fixture file
        let parsed = parse_config(CONFIG_FIXTURE).unwrap();
        let image_config = build_image_config(&args, parsed.config, 1, "amd64").unwrap();

        let rootfs = Dir::open_ambient_dir(rootfs_dir.path(), ambient_authority()).unwrap();

        // create a single empty component for testing
        let components = vec![(
            "test".to_string(),
            Component {
                mtime_clamp: 1,
                stability: 0.0,
                files: Default::default(),
            },
        )];

        // and build the OCI image
        let builder = Builder::new(&rootfs, components)
            .unwrap()
            .compression(Compression::None)
            .annotations(parsed.annotations)
            .config(image_config);
        let mut output = Vec::new();
        builder.build(&mut output).unwrap();

        // now extract it back out and open it as an ocidir
        let oci_tempdir = tempfile::tempdir().unwrap();
        let mut archive = tar::Archive::new(output.as_slice());
        archive.unpack(oci_tempdir.path()).unwrap();

        let oci_dir_cap = Dir::open_ambient_dir(oci_tempdir.path(), ambient_authority()).unwrap();
        let oci_dir = ocidir::OciDir::open(oci_dir_cap).unwrap();

        // get the image manifest (we don't set a tag, so use the index)
        let index = oci_dir.read_index().unwrap();
        let manifest_desc = index.manifests().first().unwrap();
        let manifest: oci_image::ImageManifest = oci_dir.read_json_blob(manifest_desc).unwrap();

        // get the image config and verify it matches the fixture
        let image_config: oci_image::ImageConfiguration =
            oci_dir.read_json_blob(manifest.config()).unwrap();
        let config = image_config.config().clone().unwrap();
        let expected: oci_image::Config = serde_json::from_str(CONFIG_FIXTURE).unwrap();
        assert_eq!(config, expected);

        // verify the created timestamp is set correctly (epoch 1 = 1970-01-01T00:00:01Z)
        assert_eq!(
            image_config.created().as_deref(),
            Some("1970-01-01T00:00:01Z")
        );

        // verify there's no history
        assert!(
            image_config.history().as_ref().is_none_or(|h| h.is_empty()),
            "image should have no history entries"
        );

        // verify there are no layers (empty components are filtered out)
        assert!(
            manifest.layers().is_empty(),
            "empty rootfs should have no layers"
        );
    }

    #[test]
    fn test_parse_config_direct_format() {
        // Test parsing direct OCI config format
        let json = r#"{"Entrypoint": ["/bin/sh"], "Cmd": ["-c", "echo hi"]}"#;
        let parsed = parse_config(json).unwrap();

        assert_eq!(
            parsed.config.entrypoint(),
            &Some(vec!["/bin/sh".to_string()])
        );
        assert_eq!(
            parsed.config.cmd(),
            &Some(vec!["-c".to_string(), "echo hi".to_string()])
        );
        // Direct OCI config format has no Architecture field
        assert_eq!(parsed.architecture, None);
    }

    #[test]
    fn test_parse_config_inspect_format() {
        // Test parsing podman/docker inspect format
        let json = r#"[{
            "Config": {
                "Entrypoint": ["/usr/bin/app"],
                "Env": ["PATH=/usr/bin"]
            },
            "Annotations": {
                "org.example.key": "value"
            },
            "Architecture": "arm64",
            "Created": "2023-11-14T22:13:20Z"
        }]"#;
        let parsed = parse_config(json).unwrap();

        assert_eq!(
            parsed.config.entrypoint(),
            &Some(vec!["/usr/bin/app".to_string()])
        );
        assert_eq!(
            parsed.config.env(),
            &Some(vec!["PATH=/usr/bin".to_string()])
        );
        assert_eq!(
            parsed.annotations.get("org.example.key"),
            Some(&"value".to_string())
        );
        assert_eq!(parsed.architecture, Some("arm64".to_string()));
        assert_eq!(parsed.created, Some("2023-11-14T22:13:20Z".to_string()));
    }

    #[test]
    fn test_parse_config_inspect_single_object() {
        // Test parsing a single inspect object (not wrapped in array)
        let json = r#"{"Config": {"Entrypoint": ["/bin/app"], "WorkingDir": "/data"}, "Architecture": "amd64"}"#;
        let parsed = parse_config(json).unwrap();

        assert_eq!(
            parsed.config.entrypoint(),
            &Some(vec!["/bin/app".to_string()])
        );
        assert_eq!(parsed.config.working_dir(), &Some("/data".to_string()));
        assert_eq!(parsed.architecture, Some("amd64".to_string()));
    }

    #[test]
    fn test_parse_key_value_pairs_invalid() {
        let invalid_pairs = ["", "no-equals", "=", "=value", "-key", "=-"];

        for pair in invalid_pairs {
            let pairs = vec![pair.into()];
            let result = parse_key_value_pairs(&pairs, HashMap::new());
            assert!(result.is_err(), "pair {:?} should be rejected", pair);
        }
    }

    #[test]
    fn test_parse_key_value_pairs_valid() {
        use maplit::hashmap;

        let base = hashmap! {
            "to-remove".into() => "base".into(),
            "to-override".into() => "base".into(),
            "-".into() => "dash-value".into(),
        };
        let result = parse_key_value_pairs(
            &[
                "to-remove-".into(),
                "to-override=cli".into(),
                "new=first".into(),
                "new=second".into(),
                "empty=".into(),
                "has=equals=in=value".into(),
                "nonexistent-".into(),
                "--".into(),
            ],
            base,
        )
        .unwrap();
        assert_eq!(
            result,
            hashmap! {
                "to-override".into() => "cli".into(),
                "new".into() => "second".into(),
                "empty".into() => "".into(),
                "has".into() => "equals=in=value".into(),
            }
        );
    }

    #[test]
    fn test_parse_key_value_pairs_clear() {
        use maplit::hashmap;

        // Verify "-" clears both base labels and earlier CLI pairs
        let base = hashmap! { "from-base".into() => "value".into() };
        let result = parse_key_value_pairs(
            &[
                "from-cli=value".into(),  // add via CLI
                "-".into(),               // clear all (base + CLI)
                "after-clear=new".into(), // add after clear
            ],
            base,
        )
        .unwrap();
        assert_eq!(result, hashmap! { "after-clear".into() => "new".into() });
    }

    #[test]
    fn test_build_image_config_labels_override() {
        // Base config with pre-existing labels
        let json = r#"{
            "Labels": {
                "existing": "from-config",
                "override-me": "old-value"
            }
        }"#;
        let parsed = parse_config(json).unwrap();

        // CLI labels that override one, add a new one, and later CLI label overrides earlier
        let args = BuildArgs {
            labels: vec![
                "override-me=new-value".into(),
                "new-label=first".into(),
                "new-label=second".into(), // later CLI label overrides earlier
            ],
            ..Default::default()
        };

        let image_config = build_image_config(&args, parsed.config, 1, "amd64").unwrap();
        let labels = image_config
            .config()
            .as_ref()
            .unwrap()
            .labels()
            .as_ref()
            .unwrap();

        assert_eq!(labels.get("existing"), Some(&"from-config".to_string()));
        assert_eq!(labels.get("override-me"), Some(&"new-value".to_string()));
        assert_eq!(labels.get("new-label"), Some(&"second".to_string()));
    }

    #[test]
    fn test_packing_with_xattrs() {
        use camino::Utf8Path;
        use cap_std_ext::dirext::CapStdExtDirExt;

        const MB: usize = 1024 * 1024;
        const XATTR_COMPONENT: &str = "user.component";
        const XATTR_INTERVAL: &str = "user.update-interval";

        // Helper to set up a rootfs with 3 files (3M, 2M, 1M), load components
        // with the given update intervals, and pack into 2 layers.
        let pack = |large_interval: &str, medium_interval: &str, small_interval: &str| {
            let tmp = tempfile::tempdir().unwrap();
            let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();

            let add_component = |name: &str, size_mb: usize, interval: &str| {
                rootfs.write(name, &vec![0u8; size_mb * MB]).unwrap();
                rootfs
                    .setxattr(name, XATTR_COMPONENT, name.as_bytes())
                    .unwrap();
                rootfs
                    .setxattr(name, XATTR_INTERVAL, interval.as_bytes())
                    .unwrap();
            };

            add_component("large", 3, large_interval);
            add_component("medium", 2, medium_interval);
            add_component("small", 1, small_interval);

            let files = crate::scan::Scanner::new(&rootfs).scan().unwrap();
            let repos = ComponentsRepos::load(&rootfs, &files, 0).unwrap();
            let components = repos.into_components(&rootfs, files).unwrap();
            pack_components(2, components).unwrap()
        };

        // Helper to find which packed layer contains a given file.
        let find_layer = |packed: &Vec<(String, Component)>, path: &str| -> usize {
            packed
                .iter()
                .position(|(_, c)| c.files.contains_key(Utf8Path::new(path)))
                .unwrap_or_else(|| panic!("{path} not found in any layer"))
        };

        // Scenario 1: 3M daily, 2M daily, 1M yearly
        // The 1M yearly is stable, so it should get its own layer. The 3M+2M
        // daily should merge (both unstable, lowest TEV loss).
        let packed = pack("daily", "daily", "yearly");
        assert_eq!(packed.len(), 2);
        assert_eq!(
            find_layer(&packed, "/large"),
            find_layer(&packed, "/medium"),
            "unstable large+medium should be in the same layer"
        );
        assert_ne!(
            find_layer(&packed, "/large"),
            find_layer(&packed, "/small"),
            "stable small should be separate from unstable large"
        );

        // Scenario 2: 3M daily, 2M yearly, 1M yearly
        // Now the 2M joins the stable camp. The two stable components merge
        // (low TEV loss since combined stability barely drops), and the 3M
        // daily gets its own layer.
        let packed = pack("daily", "yearly", "yearly");
        assert_eq!(packed.len(), 2);
        assert_eq!(
            find_layer(&packed, "/medium"),
            find_layer(&packed, "/small"),
            "stable medium+small should be in the same layer"
        );
        assert_ne!(
            find_layer(&packed, "/large"),
            find_layer(&packed, "/medium"),
            "unstable large should be separate from stable medium"
        );
    }
}
