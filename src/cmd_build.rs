use std::collections::HashMap;

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use cap_std_ext::cap_std::ambient_authority;
use cap_std_ext::cap_std::fs::Dir;
use clap::Parser;
use ocidir::oci_spec::image as oci_image;
use serde::Deserialize;

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

    let created_epoch = args
        .source_date_epoch
        .map_or_else(utils::get_current_epoch, Ok)?;
    tracing::debug!(created_epoch, "using timestamp");

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
        }
    };

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
    tracing::info!(files = files.len(), "scan complete");

    let repos =
        ComponentsRepos::load(&rootfs, &files, created_epoch).context("loading components")?;
    if repos.is_empty() {
        anyhow::bail!("no supported component repo found in rootfs");
    }

    let components = repos.into_components(files);
    tracing::info!(components = components.len(), "components assigned");

    // pack components down to max layers
    let components = pack_components(args, components).context("packing components")?;
    tracing::info!(layers = components.len(), "packing complete");

    // build the OCI image
    let compression = if args.compressed {
        Compression::Gzip(args.compression_level)
    } else {
        Compression::None
    };

    let builder = Builder::new(&rootfs, components)
        .context("creating builder")?
        .compression(compression)
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

    tracing::info!("build complete");
    Ok(())
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
    args: &BuildArgs,
    components: HashMap<String, Component>,
) -> Result<Vec<(String, Component)>> {
    let max_layers = args.max_layers;

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
            "Architecture": "arm64"
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
}
