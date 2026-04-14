use std::collections::HashMap;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::cap_std::fs::Dir;
use indexmap::IndexSet;
use serde::Deserialize;

use super::{ComponentId, ComponentInfo, ComponentsRepo, FileMap, STABILITY_PERIOD_DAYS};

const REPO_NAME: &str = "pathmap";

/// A single entry in the path-map file.
#[derive(Debug, Deserialize)]
pub struct PathMapEntry {
    /// Absolute path prefix to match (e.g. "/usr/share/fonts").
    ///
    /// Matching is path-component-aware, so "/usr/lib" does not match
    /// "/usr/libexec".
    pub prefix: String,
    /// Component name to assign to matching paths.
    pub component: String,
    /// Expected update cadence. Controls the packing stability weight.
    ///
    /// Valid values: "daily", "weekly", "monthly". Defaults to "monthly".
    #[serde(default = "default_interval")]
    pub interval: Interval,
}

/// Expected update cadence for a component.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Interval {
    Daily,
    Weekly,
    Monthly,
}

fn default_interval() -> Interval {
    Interval::Monthly
}

impl Interval {
    /// Convert update interval to a stability probability over STABILITY_PERIOD_DAYS.
    ///
    /// Models change events as a Poisson process at rate 1/period_days.
    /// P(no change in T days) = e^(-T / period_days), which is always in (0, 1]
    /// and strictly ordered (daily < weekly < monthly) regardless of the window.
    fn to_stability(self) -> f64 {
        let period_days = match self {
            Interval::Daily => 1.0_f64,
            Interval::Weekly => 7.0_f64,
            Interval::Monthly => 30.0_f64,
        };
        (-STABILITY_PERIOD_DAYS / period_days).exp()
    }
}

/// Per-component metadata derived from path-map entries.
struct ComponentMeta {
    stability: f64,
}

/// Path-map component repo.
///
/// Claims files based on path-prefix rules loaded from a JSON file. This is
/// useful for build systems (like BuildStream/GNOME OS) that don't ship a
/// package database but whose authors know which path prefixes belong to which
/// logical components.
///
/// Rules are evaluated in order; the first matching prefix wins. Claims are
/// weak so xattr and rpm repos take precedence.
///
/// File format: a JSON array of objects, each with:
/// - `prefix`    (string, required): absolute path prefix
/// - `component` (string, required): component name
/// - `interval`  (string, optional): "daily", "weekly", or "monthly" (default)
///
/// Example:
/// ```json
/// [
///   {"prefix": "/usr/share/fonts",   "component": "fonts",          "interval": "monthly"},
///   {"prefix": "/usr/lib/modules",   "component": "kernel-modules",  "interval": "monthly"},
///   {"prefix": "/usr/share/gnome-shell", "component": "gnome-shell", "interval": "weekly"}
/// ]
/// ```
pub struct PathmapRepo {
    /// Component names, indexed by ComponentId.
    components: IndexSet<String>,
    /// Per-component metadata (stability, etc.).
    component_meta: Vec<ComponentMeta>,
    /// Pre-computed path → ComponentId map.
    path_to_component: HashMap<Utf8PathBuf, ComponentId>,
    default_mtime_clamp: u64,
}

impl PathmapRepo {
    /// Load a path-map repo from a JSON file.
    pub fn load(map_path: &Utf8Path, files: &FileMap, default_mtime_clamp: u64) -> Result<Self> {
        let content = std::fs::read_to_string(map_path)
            .with_context(|| format!("reading path-map file {map_path}"))?;
        let entries: Vec<PathMapEntry> = serde_json::from_str(&content)
            .with_context(|| format!("parsing path-map file {map_path}"))?;

        let mut components: IndexSet<String> = IndexSet::new();
        let mut component_meta: Vec<ComponentMeta> = Vec::new();
        let mut path_to_component: HashMap<Utf8PathBuf, ComponentId> = HashMap::new();

        // Pre-intern all component names and their metadata so that we can look
        // them up by ComponentId during the main loop without re-computing the
        // interval → stability mapping for every file.
        let mut entry_ids: Vec<ComponentId> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let stability = entry.interval.to_stability();
            let (idx, inserted) = components.insert_full(entry.component.clone());
            if inserted {
                component_meta.push(ComponentMeta { stability });
            }
            entry_ids.push(ComponentId(idx));
        }

        for file_path in files.keys() {
            for (entry, &comp_id) in entries.iter().zip(entry_ids.iter()) {
                let prefix = Utf8Path::new(&entry.prefix);
                if file_path.starts_with(prefix) {
                    path_to_component.insert(file_path.clone(), comp_id);
                    break; // first matching rule wins
                }
            }
        }

        tracing::debug!(
            path = %map_path,
            rules = entries.len(),
            components = components.len(),
            paths = path_to_component.len(),
            "loaded pathmap components"
        );

        Ok(Self {
            components,
            component_meta,
            path_to_component,
            default_mtime_clamp,
        })
    }
}

impl ComponentsRepo for PathmapRepo {
    fn name(&self) -> &'static str {
        REPO_NAME
    }

    fn default_priority(&self) -> usize {
        50
    }

    fn weak_claims_for_path(
        &self,
        _rootfs: &Dir,
        path: &Utf8Path,
        _file_info: &super::FileInfo,
    ) -> Result<Vec<ComponentId>> {
        Ok(self
            .path_to_component
            .get(path)
            .map(|id| vec![*id])
            .unwrap_or_default())
    }

    fn component_info(&self, id: ComponentId) -> ComponentInfo<'_> {
        ComponentInfo {
            name: self
                .components
                .get_index(id.0)
                .expect("invalid ComponentId"),
            mtime_clamp: self.default_mtime_clamp,
            stability: self.component_meta[id.0].stability,
        }
    }
}

#[cfg(test)]
mod tests {
    use cap_std_ext::cap_std::ambient_authority;
    use cap_std_ext::cap_std::fs::Dir;

    use super::*;
    use crate::components::{FileMap, FileType};

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

    fn fi(file_type: FileType) -> crate::components::FileInfo {
        crate::components::FileInfo::dummy(file_type)
    }

    fn assert_component(repo: &PathmapRepo, rootfs: &Dir, path: &str, expected: &str) {
        let claims = repo
            .weak_claims_for_path(rootfs, Utf8Path::new(path), &fi(FileType::File))
            .unwrap();
        assert_eq!(claims.len(), 1, "{path} should have exactly one claim");
        assert_eq!(
            repo.component_info(claims[0]).name,
            expected,
            "{path} should be claimed by {expected}"
        );
    }

    fn write_map(tmp: &tempfile::TempDir, json: &str) -> camino::Utf8PathBuf {
        let path = tmp.path().join("pathmap.json");
        std::fs::write(&path, json).unwrap();
        camino::Utf8PathBuf::try_from(path).unwrap()
    }

    #[test]
    fn test_pathmap_basic() {
        let map_tmp = tempfile::tempdir().unwrap();
        let map_path = write_map(
            &map_tmp,
            r#"[
                {"prefix": "/usr/share/fonts", "component": "fonts"},
                {"prefix": "/usr/lib/modules", "component": "kernel-modules", "interval": "monthly"}
            ]"#,
        );

        let (_tmp, files) = setup_rootfs(|rootfs| {
            rootfs.create_dir_all("usr/share/fonts/dejavu").unwrap();
            rootfs
                .write("usr/share/fonts/dejavu/DejaVuSans.ttf", "font")
                .unwrap();
            rootfs.create_dir_all("usr/lib/modules/6.12").unwrap();
            rootfs
                .write("usr/lib/modules/6.12/vmlinuz", "kernel")
                .unwrap();
            rootfs.create_dir_all("usr/bin").unwrap();
            rootfs.write("usr/bin/bash", "bash").unwrap();
        });

        let rootfs_dir = Dir::open_ambient_dir(_tmp.path(), ambient_authority()).unwrap();

        let repo = PathmapRepo::load(&map_path, &files, 0).unwrap();

        assert_component(
            &repo,
            &rootfs_dir,
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "fonts",
        );
        assert_component(
            &repo,
            &rootfs_dir,
            "/usr/lib/modules/6.12/vmlinuz",
            "kernel-modules",
        );

        // unclaimed path should return no claims
        let claims = repo
            .weak_claims_for_path(
                &rootfs_dir,
                Utf8Path::new("/usr/bin/bash"),
                &fi(FileType::File),
            )
            .unwrap();
        assert!(claims.is_empty(), "/usr/bin/bash should not be claimed");
    }

    #[test]
    fn test_pathmap_first_rule_wins() {
        let map_tmp = tempfile::tempdir().unwrap();
        let map_path = write_map(
            &map_tmp,
            r#"[
                {"prefix": "/usr/share/gnome-shell", "component": "gnome-shell"},
                {"prefix": "/usr/share",             "component": "share-data"}
            ]"#,
        );

        let (_tmp, files) = setup_rootfs(|rootfs| {
            rootfs
                .create_dir_all("usr/share/gnome-shell/extensions")
                .unwrap();
            rootfs
                .write("usr/share/gnome-shell/extensions/ext.js", "js")
                .unwrap();
            rootfs.create_dir_all("usr/share/icons").unwrap();
            rootfs.write("usr/share/icons/foo.svg", "svg").unwrap();
        });

        let rootfs_dir = Dir::open_ambient_dir(_tmp.path(), ambient_authority()).unwrap();
        let repo = PathmapRepo::load(&map_path, &files, 0).unwrap();

        // more specific rule wins
        assert_component(
            &repo,
            &rootfs_dir,
            "/usr/share/gnome-shell/extensions/ext.js",
            "gnome-shell",
        );
        // falls through to the broader rule
        assert_component(&repo, &rootfs_dir, "/usr/share/icons/foo.svg", "share-data");
    }

    #[test]
    fn test_pathmap_no_prefix_bleed() {
        // /usr/lib should not match /usr/libexec
        let map_tmp = tempfile::tempdir().unwrap();
        let map_path = write_map(&map_tmp, r#"[{"prefix": "/usr/lib", "component": "libs"}]"#);

        let (_tmp, files) = setup_rootfs(|rootfs| {
            rootfs.create_dir_all("usr/lib").unwrap();
            rootfs.write("usr/lib/libfoo.so", "lib").unwrap();
            rootfs.create_dir_all("usr/libexec").unwrap();
            rootfs.write("usr/libexec/helper", "helper").unwrap();
        });

        let rootfs_dir = Dir::open_ambient_dir(_tmp.path(), ambient_authority()).unwrap();
        let repo = PathmapRepo::load(&map_path, &files, 0).unwrap();

        assert_component(&repo, &rootfs_dir, "/usr/lib/libfoo.so", "libs");

        let claims = repo
            .weak_claims_for_path(
                &rootfs_dir,
                Utf8Path::new("/usr/libexec/helper"),
                &fi(FileType::File),
            )
            .unwrap();
        assert!(
            claims.is_empty(),
            "/usr/libexec/helper must not bleed into /usr/lib rule"
        );
    }

    #[test]
    fn test_interval_stability() {
        // daily components are less stable than monthly ones
        assert!(Interval::Daily.to_stability() < Interval::Weekly.to_stability());
        assert!(Interval::Weekly.to_stability() < Interval::Monthly.to_stability());
        // monthly: e^(-7/30) ≈ 0.794
        let monthly = Interval::Monthly.to_stability();
        assert!((monthly - (-STABILITY_PERIOD_DAYS / 30.0_f64).exp()).abs() < 1e-9);
        // daily: e^(-7/1) ≈ 0.0009 — non-zero but very low
        assert!(Interval::Daily.to_stability() > 0.0);
    }
}
