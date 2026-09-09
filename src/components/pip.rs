use std::collections::HashMap;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::cap_std::fs::Dir;
use indexmap::IndexSet;

use super::{
    ComponentId, ComponentInfo, ComponentsRepo, FileInfo, FileMap, FileType, interval_to_stability,
};
use crate::utils::{
    canonicalize_parent_path, normalize_path, read_file_contents_to_string_checked,
};

const REPO_NAME: &str = "pip";

/// Suffix of the per-distribution metadata directory.
const DIST_INFO_SUFFIX: &str = ".dist-info";

/// The installed-files manifest inside a `.dist-info` directory.
const RECORD_FILENAME: &str = "RECORD";

/// Directory holding compiled bytecode next to its source files.
const PYCACHE_DIR: &str = "__pycache__";

/// Suffix of compiled bytecode files.
const PYC_SUFFIX: &str = ".pyc";

/// Maximum size of a RECORD file we're willing to read into memory.
const RECORD_MAX_SIZE: u64 = 64 * 1024 * 1024;

/// Python packages don't carry changelog data, so assume a fixed update
/// cadence for stability purposes. PyPI packages tend to update more often
/// than distro packages.
const DEFAULT_UPDATE_INTERVAL_DAYS: u64 = 30;

/// Python distributions components repo.
///
/// Claims files listed in `*.dist-info/RECORD` files found anywhere in the
/// rootfs, as specified by PEP 376 ("Recording installed projects",
/// <https://packaging.python.org/en/latest/specifications/recording-installed-packages/>).
/// Any installer that follows the spec (pip, uv, ...) is supported.
///
/// Distributions without a RECORD are ignored. Per the spec, distro package
/// managers remove RECORD to mark a distribution as managed by them (e.g.
/// Fedora), in which case the rpm/alpm repos own those files anyway.
///
/// Distributions with the same normalized name found in multiple locations
/// (e.g. multiple Python versions or venvs) are merged into one component.
/// Distributions vendored inside another distribution (their dist-info is
/// listed in the outer RECORD, e.g. `setuptools/_vendor`, see
/// <https://github.com/pypa/setuptools/issues/2825>) belong to the outer one.
pub struct PipRepo {
    /// Normalized distribution names, indexed by ComponentId.
    components: IndexSet<String>,
    /// Mapping from path to the component(s) that claim it. Directories may be
    /// shared between multiple distributions (e.g. namespace packages).
    path_to_components: HashMap<Utf8PathBuf, Vec<ComponentId>>,
    default_mtime_clamp: u64,
    stability: f64,
}

impl PipRepo {
    /// Load the pip repo by finding all RECORD files in the scanned rootfs.
    /// Returns `None` if no Python distributions are installed.
    pub fn load(rootfs: &Dir, files: &FileMap, default_mtime_clamp: u64) -> Result<Option<Self>> {
        let mut components: IndexSet<String> = IndexSet::new();
        let mut path_to_components: HashMap<Utf8PathBuf, Vec<ComponentId>> = HashMap::new();
        let mut canonicalization_cache = HashMap::new();
        let mut n_dists = 0usize;
        let mut n_vendored = 0usize;

        // site-packages (or equivalent) is the parent of the dist-info dir
        // and RECORD entries are relative to it.
        let mut dist_infos: Vec<(&Utf8Path, &Utf8Path, String)> = files
            .iter()
            .filter(|(_, file_info)| file_info.file_type == FileType::File)
            .filter_map(|(path, _)| {
                let (dist_info_dir, name) = parse_record_path(path)?;
                Some((path.as_path(), dist_info_dir.parent()?, name))
            })
            .collect();

        // Process outermost first, so that a distribution vendored inside
        // another one (its RECORD is listed in the outer RECORD) is already
        // claimed by the time we get to it and can be skipped.
        dist_infos.sort_by_key(|(_, site_packages, _)| site_packages.components().count());

        for (record_path, site_packages, name) in dist_infos {
            if path_to_components.contains_key(record_path) {
                tracing::trace!(path = %record_path, "skipping vendored dist-info");
                n_vendored += 1;
                continue;
            }

            let record = match read_record(rootfs, record_path) {
                Ok(record) => record,
                Err(err) => {
                    tracing::warn!(path = %record_path, error = %err, "skipping unreadable RECORD");
                    continue;
                }
            };

            let (id, created) = components.insert_full(name.clone());
            let id = ComponentId(id);
            if created {
                tracing::trace!(component = %name, id = id.0, "pip component created");
            }
            n_dists += 1;
            tracing::debug!(component = %name, record = %record_path, entries = record.len(), "loaded RECORD");

            for rel in record {
                let joined = site_packages.join(&rel);
                let abs = normalize_path(&joined)
                    .with_context(|| format!("normalizing {joined} from {record_path}"))?;
                let canonical =
                    canonicalize_parent_path(rootfs, files, &abs, &mut canonicalization_cache)
                        .with_context(|| format!("canonicalizing {abs}"))?;
                if canonical != abs {
                    tracing::trace!(original = %abs, canonical = %canonical, "path canonicalized");
                }
                if !files.contains_key(&canonical) {
                    tracing::trace!(path = %canonical, component = %name, "RECORD entry not in rootfs");
                    continue;
                }
                claim(&mut path_to_components, canonical.clone(), id);

                // claim parent directories up to (but excluding) site-packages
                for dir in canonical
                    .ancestors()
                    .skip(1)
                    .take_while(|dir| dir.starts_with(site_packages) && *dir != site_packages)
                {
                    claim(&mut path_to_components, dir.to_owned(), id);
                }
            }
        }

        if components.is_empty() {
            return Ok(None);
        }

        // Bytecode may have been compiled after installation (e.g. by
        // `compileall`), in which case it isn't in RECORD. Claim it for the
        // component that owns the corresponding source file.
        let mut n_pyc = 0usize;
        for (path, file_info) in files {
            if file_info.file_type != FileType::File || path_to_components.contains_key(path) {
                continue;
            }
            let Some(source) = pyc_source_path(path) else {
                continue;
            };
            let Some(ids) = path_to_components.get(&source).cloned() else {
                continue;
            };
            tracing::trace!(path = %path, source = %source, "claiming orphan bytecode");
            for id in ids {
                claim(&mut path_to_components, path.clone(), id);
                // and the __pycache__ dir itself
                if let Some(dir) = path.parent() {
                    claim(&mut path_to_components, dir.to_owned(), id);
                }
            }
            n_pyc += 1;
        }

        tracing::debug!(
            dists = n_dists,
            vendored = n_vendored,
            components = components.len(),
            paths = path_to_components.len(),
            orphan_pyc = n_pyc,
            "loaded pip components"
        );

        Ok(Some(Self {
            components,
            path_to_components,
            default_mtime_clamp,
            stability: interval_to_stability(DEFAULT_UPDATE_INTERVAL_DAYS),
        }))
    }
}

impl ComponentsRepo for PipRepo {
    fn name(&self) -> &'static str {
        REPO_NAME
    }

    /// Below rpm/alpm so that Python packages installed by the distro package
    /// manager stay with their distro package.
    fn default_priority(&self) -> usize {
        20
    }

    fn strong_claims_for_path(&self, path: &Utf8Path, _file_info: &FileInfo) -> Vec<ComponentId> {
        self.path_to_components
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    fn component_info(&self, id: ComponentId) -> ComponentInfo<'_> {
        ComponentInfo {
            name: &self.components[id.0],
            mtime_clamp: self.default_mtime_clamp,
            stability: self.stability,
        }
    }
}

/// If `path` is `<...>/<name>-<version>.dist-info/RECORD`, returns the
/// dist-info directory and the normalized distribution name.
///
/// The spec requires the directory name to be `{name}-{version}` with both
/// fields normalized, so the name contains no `-` and the first `-` is the
/// separator.
fn parse_record_path(path: &Utf8Path) -> Option<(&Utf8Path, String)> {
    if path.file_name() != Some(RECORD_FILENAME) {
        return None;
    }
    let dist_info_dir = path.parent()?;
    let stem = dist_info_dir.file_name()?.strip_suffix(DIST_INFO_SUFFIX)?;
    let (name, _version) = stem.split_once('-')?;
    if name.is_empty() {
        return None;
    }
    Some((dist_info_dir, normalize_name(name)))
}

/// Normalize a distribution name per PEP 503.
fn normalize_name(name: &str) -> String {
    name.split(['-', '_', '.'])
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("-")
}

/// Read a RECORD file and return the paths listed in it.
fn read_record(rootfs: &Dir, path: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let rel = path.strip_prefix("/").unwrap_or(path);
    let mut file = rootfs.open(rel).context("opening RECORD")?;
    let contents = read_file_contents_to_string_checked(&mut file, RECORD_MAX_SIZE)
        .context("reading RECORD")?;
    Ok(parse_record(&contents))
}

/// Parse RECORD contents (CSV: `path,hash,size`) into a list of paths.
///
/// Paths are absolute or relative to the `.dist-info` directory's parent
/// (i.e. site-packages). Blank lines are ignored.
fn parse_record(contents: &str) -> Vec<Utf8PathBuf> {
    contents
        .lines()
        .filter_map(|line| {
            let path = csv_first_field(line);
            (!path.is_empty()).then(|| Utf8PathBuf::from(path))
        })
        .collect()
}

/// Extract the first field of a CSV line, handling double-quoted fields
/// (used by pip when the path contains a comma or quote).
fn csv_first_field(line: &str) -> String {
    let Some(quoted) = line.strip_prefix('"') else {
        return line
            .split_once(',')
            .map_or(line, |(field, _)| field)
            .to_owned();
    };
    let mut out = String::new();
    let mut chars = quoted.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if chars.next_if_eq(&'"').is_some() => out.push('"'),
            '"' => break,
            c => out.push(c),
        }
    }
    out
}

/// Add `id` as a claimant of `path`, without duplicates.
fn claim(map: &mut HashMap<Utf8PathBuf, Vec<ComponentId>>, path: Utf8PathBuf, id: ComponentId) {
    let ids = map.entry(path).or_default();
    if !ids.contains(&id) {
        ids.push(id);
    }
}

/// If `path` is `<dir>/__pycache__/<module>.<tag>.pyc`, returns
/// `<dir>/<module>.py`.
fn pyc_source_path(path: &Utf8Path) -> Option<Utf8PathBuf> {
    let filename = path.file_name()?.strip_suffix(PYC_SUFFIX)?;
    let pycache = path.parent()?;
    if pycache.file_name() != Some(PYCACHE_DIR) {
        return None;
    }
    // The format is `<module>.<tag>[.opt-N]`. Strip from the right since
    // module filenames can contain dots (even if they aren't importable)
    let filename = filename.rsplit_once(".opt-").map_or(filename, |(f, _)| f);
    let (module, _tag) = filename.rsplit_once('.')?;
    if module.is_empty() {
        return None;
    }
    Some(pycache.parent()?.join(format!("{module}.py")))
}

#[cfg(test)]
mod tests {
    use cap_std_ext::cap_std::ambient_authority;

    use super::*;

    /// Helper to set up a rootfs, run setup, and scan files.
    fn setup_rootfs<F>(setup: F) -> (tempfile::TempDir, Dir, FileMap)
    where
        F: FnOnce(&Dir),
    {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = Dir::open_ambient_dir(tmp.path(), ambient_authority()).unwrap();
        setup(&rootfs);
        let files = crate::scan::Scanner::new(&rootfs).scan().unwrap();
        (tmp, rootfs, files)
    }

    /// Helper to write a file, creating parent directories as needed.
    fn write(rootfs: &Dir, path: &str, contents: impl AsRef<[u8]>) {
        if let Some(parent) = Utf8Path::new(path).parent() {
            rootfs.create_dir_all(parent).unwrap();
        }
        rootfs.write(path, contents).unwrap();
    }

    /// Helper to assert which components claim a path.
    fn assert_claims(repo: &PipRepo, files: &FileMap, path: &str, expected: &[&str]) {
        let path = Utf8Path::new(path);
        let mut names: Vec<&str> = repo
            .strong_claims_for_path(path, &files[path])
            .into_iter()
            .map(|id| repo.component_info(id).name)
            .collect();
        names.sort_unstable();
        assert_eq!(names, expected, "claims for {path}");
    }

    #[test]
    fn test_load_and_claims() {
        let (_tmp, rootfs, files) = setup_rootfs(|rootfs| {
            // a package with bytecode and a console script. The bytecode
            // for __init__ was compiled after install and is not in RECORD
            write(rootfs, "site-packages/foo/__init__.py", "");
            write(rootfs, "site-packages/foo/bar.py", "");
            write(
                rootfs,
                "site-packages/foo/__pycache__/__init__.cpython-312.pyc",
                "",
            );
            write(
                rootfs,
                "site-packages/foo/__pycache__/bar.cpython-312.pyc",
                "",
            );
            write(rootfs, "bin/foo-cli", "");
            write(
                rootfs,
                "site-packages/Foo_Pkg-1.2.3.dist-info/RECORD",
                "foo/__init__.py,sha256=abc,0\n\
                 foo/bar.py,sha256=abc,0\n\
                 foo/__pycache__/bar.cpython-312.pyc,,\n\
                 ../bin/foo-cli,sha256=abc,0\n\
                 Foo_Pkg-1.2.3.dist-info/RECORD,,\n\
                 foo/missing.py,sha256=abc,0\n", // not in rootfs
            );

            // a second package sharing the foo directory, with an absolute path
            write(rootfs, "site-packages/foo/baz.py", "");
            write(rootfs, "site-packages/absolute.py", "");
            write(
                rootfs,
                "site-packages/baz-0.1.dist-info/RECORD",
                "foo/baz.py,,\nbaz-0.1.dist-info/RECORD,,\n/site-packages/absolute.py,,\n",
            );

            // unrelated file
            write(rootfs, "site-packages/README.txt", "");
        });
        let repo = PipRepo::load(&rootfs, &files, 42).unwrap().unwrap();
        assert_eq!(repo.components.len(), 2);

        assert_claims(&repo, &files, "/site-packages/foo/bar.py", &["foo-pkg"]);
        assert_claims(
            &repo,
            &files,
            "/site-packages/foo/__pycache__/bar.cpython-312.pyc",
            &["foo-pkg"],
        );
        // orphan bytecode claimed via its source
        assert_claims(
            &repo,
            &files,
            "/site-packages/foo/__pycache__/__init__.cpython-312.pyc",
            &["foo-pkg"],
        );
        assert_claims(
            &repo,
            &files,
            "/site-packages/foo/__pycache__",
            &["foo-pkg"],
        );
        // console script via `..`
        assert_claims(&repo, &files, "/bin/foo-cli", &["foo-pkg"]);
        // dist-info claimed too
        assert_claims(
            &repo,
            &files,
            "/site-packages/Foo_Pkg-1.2.3.dist-info/RECORD",
            &["foo-pkg"],
        );
        assert_claims(
            &repo,
            &files,
            "/site-packages/Foo_Pkg-1.2.3.dist-info",
            &["foo-pkg"],
        );
        // shared directory claimed by both
        assert_claims(&repo, &files, "/site-packages/foo", &["baz", "foo-pkg"]);
        // absolute path
        assert_claims(&repo, &files, "/site-packages/absolute.py", &["baz"]);
        // site-packages itself, unrelated files and dirs outside it are unclaimed
        assert_claims(&repo, &files, "/site-packages", &[]);
        assert_claims(&repo, &files, "/site-packages/README.txt", &[]);
        assert_claims(&repo, &files, "/bin", &[]);

        let info = repo.component_info(ComponentId(0));
        assert_eq!(info.mtime_clamp, 42);
        assert!(info.stability > 0.0 && info.stability < 1.0);
    }

    #[test]
    fn test_load_vendored_dist_info_belongs_to_outer() {
        let (_tmp, rootfs, files) = setup_rootfs(|rootfs| {
            write(rootfs, "site-packages/outer/_vendor/inner/mod.py", "");
            write(
                rootfs,
                "site-packages/outer/_vendor/inner-1.dist-info/RECORD",
                "inner/mod.py,,\ninner-1.dist-info/RECORD,,\n",
            );
            write(
                rootfs,
                "site-packages/outer-1.dist-info/RECORD",
                "outer/_vendor/inner/mod.py,,\n\
                 outer/_vendor/inner-1.dist-info/RECORD,,\n\
                 outer-1.dist-info/RECORD,,\n",
            );
        });
        let repo = PipRepo::load(&rootfs, &files, 0).unwrap().unwrap();
        assert_eq!(repo.components.len(), 1);
        assert_claims(
            &repo,
            &files,
            "/site-packages/outer/_vendor/inner/mod.py",
            &["outer"],
        );
    }

    #[test]
    fn test_load_skips_invalid_record() {
        let (_tmp, rootfs, files) = setup_rootfs(|rootfs| {
            write(
                rootfs,
                "site-packages/bad-1.dist-info/RECORD",
                [0xffu8, 0xfe], // not valid UTF-8
            );
            write(
                rootfs,
                "site-packages/good-1.dist-info/RECORD",
                "good-1.dist-info/RECORD,,\n",
            );
        });
        let repo = PipRepo::load(&rootfs, &files, 0).unwrap().unwrap();
        assert_eq!(repo.components.len(), 1);
        assert_eq!(repo.components[0], "good");
    }

    #[test]
    fn test_load_none_without_dist_info() {
        let (_tmp, rootfs, files) = setup_rootfs(|rootfs| write(rootfs, "bin/foo", ""));
        assert!(PipRepo::load(&rootfs, &files, 0).unwrap().is_none());
    }

    #[test]
    fn test_parse_record_path() {
        let p = |s: &str| parse_record_path(Utf8Path::new(s)).map(|(_, n)| n);
        assert_eq!(
            p("/usr/lib/python3.12/site-packages/Foo_Bar-1.0.dist-info/RECORD"),
            Some("foo-bar".into())
        );
        // no version
        assert_eq!(p("/opt/venv/lib/x/foo.dist-info/RECORD"), None);
        // not a RECORD file
        assert_eq!(p("/x/foo-1.0.dist-info/METADATA"), None);
        // legacy egg-info
        assert_eq!(p("/x/foo-1.0.egg-info/RECORD"), None);
        // empty name
        assert_eq!(p("/x/-1.0.dist-info/RECORD"), None);
    }

    #[test]
    fn test_normalize_name() {
        assert_eq!(normalize_name("Foo_Bar"), "foo-bar");
        assert_eq!(normalize_name("foo.bar__baz"), "foo-bar-baz");
        assert_eq!(normalize_name("numpy"), "numpy");
    }

    #[test]
    fn test_parse_record() {
        let record = "a/b.py,sha256=x,1\r\n\"c,d.py\",,\n\n\"e\"\"f\",,\n";
        assert_eq!(
            parse_record(record),
            vec![
                Utf8PathBuf::from("a/b.py"),
                Utf8PathBuf::from("c,d.py"),
                Utf8PathBuf::from("e\"f"),
            ]
        );
    }

    #[test]
    fn test_pyc_source_path() {
        let p = |s: &str| pyc_source_path(Utf8Path::new(s));
        assert_eq!(
            p("/site-packages/foo/__pycache__/bar.cpython-312.pyc"),
            Some(Utf8PathBuf::from("/site-packages/foo/bar.py"))
        );
        assert_eq!(
            p("/site-packages/foo/__pycache__/bar.cpython-312.opt-1.pyc"),
            Some(Utf8PathBuf::from("/site-packages/foo/bar.py"))
        );
        // dotted module filename
        assert_eq!(
            p("/site-packages/foo/__pycache__/bar.v2.cpython-312.opt-2.pyc"),
            Some(Utf8PathBuf::from("/site-packages/foo/bar.v2.py"))
        );
        // not in __pycache__
        assert_eq!(p("/site-packages/foo/bar.pyc"), None);
        // not bytecode
        assert_eq!(p("/site-packages/foo/__pycache__/bar.py"), None);
    }
}
