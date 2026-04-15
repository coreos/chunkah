use std::collections::HashMap;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::cap_std::fs::Dir;
use indexmap::IndexSet;
use serde::Deserialize;

use super::{ComponentId, ComponentInfo, ComponentsRepo, FileMap, STABILITY_PERIOD_DAYS};

const REPO_NAME: &str = "filemap";

/// Well-known path inside the rootfs where the filemap is stored.
///
/// Build systems (e.g. BuildStream) write their component–file mapping here at
/// build time so chunkah can auto-detect it — no CLI flag required.
pub const FILEMAP_PATH: &str = "usr/lib/chunkah/filemap.json";

/// Per-component entry in the file-map JSON.
///
/// ```json
/// {
///   "gnome-shell": { "interval": "weekly",  "files": ["/usr/bin/gnome-shell", ...] },
///   "glibc":       { "interval": "monthly", "files": ["/usr/lib64/libc.so.6", ...] }
/// }
/// ```
#[derive(Debug, Deserialize)]
pub struct FileMapEntry {
    /// How often this component typically changes.
    #[serde(default = "default_interval")]
    pub interval: Interval,
    /// Exact absolute paths belonging to this component.
    pub files: Vec<String>,
}

fn default_interval() -> Interval {
    Interval::Monthly
}

/// Update cadence for stability calculations.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Interval {
    Daily,
    Weekly,
    Monthly,
}

impl Interval {
    fn period_days(self) -> f64 {
        match self {
            Interval::Daily => 1.0,
            Interval::Weekly => 7.0,
            Interval::Monthly => 30.0,
        }
    }

    /// Stability score: exponential decay `e^(-T/period)` where T =
    /// `STABILITY_PERIOD_DAYS`.  This keeps daily < weekly < monthly strictly
    /// ordered and never collapses to 0.
    fn stability(self) -> f64 {
        (-STABILITY_PERIOD_DAYS / self.period_days()).exp()
    }
}

/// Per-component metadata derived from file-map entries.
struct ComponentMeta {
    name: String,
    stability: f64,
}

/// Claims files based on an exact file-path map embedded in the rootfs.
///
/// Intended for build systems such as BuildStream that have full file-level
/// provenance but cannot preserve xattrs through OCI export.  The map is
/// written to [`FILEMAP_PATH`] at build time and auto-detected here — no CLI
/// flag required, analogous to how the RPM backend detects the rpmdb.
///
/// **File format** — a JSON object keyed by component name:
///
/// ```json
/// {
///   "gnome-shell": {
///     "interval": "weekly",
///     "files": ["/usr/bin/gnome-shell", "/usr/lib/gnome-shell/gnome-shell"]
///   },
///   "glibc": {
///     "interval": "monthly",
///     "files": ["/usr/lib64/libc.so.6"]
///   }
/// }
/// ```
pub struct FilemapRepo {
    components: Vec<ComponentMeta>,
    path_to_component: HashMap<Utf8PathBuf, ComponentId>,
}

impl FilemapRepo {
    /// Detect and load the filemap from `rootfs`.
    ///
    /// Returns `Ok(None)` if [`FILEMAP_PATH`] is absent — same convention as
    /// [`super::rpm::RpmRepo::load`].
    pub fn load(rootfs: &Dir, files: &FileMap, _default_mtime_clamp: u64) -> Result<Option<Self>> {
        if !rootfs
            .try_exists(FILEMAP_PATH)
            .with_context(|| format!("checking for {FILEMAP_PATH}"))?
        {
            return Ok(None);
        }

        let content = rootfs
            .read_to_string(FILEMAP_PATH)
            .with_context(|| format!("reading {FILEMAP_PATH}"))?;

        let repo = Self::load_from_str(&content, files)
            .with_context(|| format!("parsing {FILEMAP_PATH}"))?;

        tracing::info!(
            path = FILEMAP_PATH,
            components = repo.components.len(),
            paths = repo.path_to_component.len(),
            "loaded filemap"
        );

        Ok(Some(repo))
    }

    /// Parse a filemap from a JSON string.  Used by `load()` and tests.
    fn load_from_str(content: &str, files: &FileMap) -> Result<Self> {
        let entries: HashMap<String, FileMapEntry> =
            serde_json::from_str(content).context("deserializing filemap JSON")?;

        let mut component_index: IndexSet<String> = IndexSet::new();
        let mut stabilities: Vec<f64> = Vec::new();
        let mut path_to_component: HashMap<Utf8PathBuf, ComponentId> = HashMap::new();

        for (name, entry) in &entries {
            let (comp_idx, _) = component_index.insert_full(name.clone());
            if comp_idx >= stabilities.len() {
                stabilities.push(entry.interval.stability());
            }
            let comp_id = ComponentId(comp_idx);

            for path_str in &entry.files {
                let path = Utf8PathBuf::from(path_str);
                if files.contains_key(&path) {
                    path_to_component.insert(path, comp_id);
                }
            }
        }

        let components: Vec<ComponentMeta> = component_index
            .into_iter()
            .zip(stabilities)
            .map(|(name, stability)| ComponentMeta { name, stability })
            .collect();

        Ok(Self {
            components,
            path_to_component,
        })
    }
}

impl ComponentsRepo for FilemapRepo {
    fn name(&self) -> &'static str {
        REPO_NAME
    }

    /// Priority 5: below xattr (0) but above rpm (10).  BST-generated file maps
    /// have build-time provenance; treat them as authoritative.
    fn default_priority(&self) -> usize {
        5
    }

    fn strong_claims_for_path(&self, path: &Utf8Path, _file_info: &super::FileInfo) -> Vec<ComponentId> {
        self.path_to_component
            .get(path)
            .copied()
            .into_iter()
            .collect()
    }

    fn component_info(&self, id: ComponentId) -> ComponentInfo<'_> {
        let meta = &self.components[id.0];
        ComponentInfo {
            name: &meta.name,
            mtime_clamp: 0,
            stability: meta.stability,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_repo(json: &str) -> FilemapRepo {
        let files: FileMap = [
            "/usr/bin/gnome-shell",
            "/usr/lib/gnome-shell/gnome-shell",
            "/usr/lib64/libc.so.6",
            "/usr/share/doc/glibc/README",
        ]
        .iter()
        .map(|p| {
            (
                Utf8PathBuf::from(*p),
                super::super::FileInfo::dummy(super::super::FileType::File),
            )
        })
        .collect();
        FilemapRepo::load_from_str(json, &files).unwrap()
    }

    #[test]
    fn basic_claims() {
        let repo = make_repo(
            r#"{
              "gnome-shell": { "interval": "weekly",  "files": ["/usr/bin/gnome-shell", "/usr/lib/gnome-shell/gnome-shell"] },
              "glibc":       { "interval": "monthly", "files": ["/usr/lib64/libc.so.6"] }
            }"#,
        );

        let shell_id = repo
            .path_to_component
            .get(Utf8Path::new("/usr/bin/gnome-shell"))
            .copied()
            .unwrap();
        assert_eq!(repo.components[shell_id.0].name, "gnome-shell");

        let glibc_id = repo
            .path_to_component
            .get(Utf8Path::new("/usr/lib64/libc.so.6"))
            .copied()
            .unwrap();
        assert_eq!(repo.components[glibc_id.0].name, "glibc");

        assert!(repo
            .path_to_component
            .get(Utf8Path::new("/usr/share/doc/glibc/README"))
            .is_none());
    }

    #[test]
    fn stability_ordering() {
        let daily = Interval::Daily.stability();
        let weekly = Interval::Weekly.stability();
        let monthly = Interval::Monthly.stability();
        assert!(daily < weekly);
        assert!(weekly < monthly);
        assert!(daily > 0.0);
    }

    #[test]
    fn default_interval_is_monthly() {
        let repo = make_repo(r#"{ "myapp": { "files": ["/usr/bin/gnome-shell"] } }"#);
        let id = repo
            .path_to_component
            .get(Utf8Path::new("/usr/bin/gnome-shell"))
            .copied()
            .unwrap();
        assert!(
            (repo.components[id.0].stability - Interval::Monthly.stability()).abs() < 1e-9
        );
    }

    #[test]
    fn unknown_paths_ignored() {
        let repo = make_repo(r#"{ "ghost": { "files": ["/does/not/exist"] } }"#);
        assert!(repo.path_to_component.is_empty());
    }
}
