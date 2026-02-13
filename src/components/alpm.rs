use anyhow::{Context, Result, anyhow, bail};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::cap_std::fs::Dir;
use indexmap::IndexMap;
use std::{collections::HashMap, io::Read, str::FromStr};

use crate::components::{ComponentId, ComponentInfo, ComponentsRepo, FileMap, FileType};

const REPO_NAME: &str = "alpm";
const LOCALDB_PATHS: &[&str] = &["usr/lib/sysimage/lib/pacman/local", "var/lib/pacman/local"];

const DESC_FILENAME: &str = "desc";
const FILES_FILENAME: &str = "files";

pub struct AlpmComponentsRepo {
    /// Unique component (BASE) names mapped to buildtime, indexed by ComponentId.
    components: IndexMap<String, u64>,

    /// Mapping from path to list of ComponentId.
    ///
    /// It's common for directories to be owned by more than one component (i.e.
    /// from _different_ packages).
    path_to_components: HashMap<Utf8PathBuf, Vec<ComponentId>>,
}

impl AlpmComponentsRepo {
    pub fn load(rootfs: &Dir, files: &FileMap) -> Result<Option<Self>> {
        let local_db = match Self::try_open_local_db(rootfs) {
            Some(dir) => dir,
            None => return Ok(None),
        };
        Self::load_from_db(&local_db, files).map(Some)
    }

    fn try_open_local_db(rootfs: &Dir) -> Option<Dir> {
        for local_db_path in LOCALDB_PATHS {
            if let Ok(dir) = rootfs.open_dir(*local_db_path) {
                return Some(dir);
            }
        }
        None
    }

    /// Starting from the `local_db` base directory, iterate over the packages in the local database,
    /// process package metadata and generate an index of components and their files.
    pub fn load_from_db(local_db: &Dir, image_files: &FileMap) -> Result<Self> {
        let mut components = IndexMap::new();
        let mut path_to_components = HashMap::new();

        // The local package database is basically a directory that contains
        // one directory for each locally installed package. Inside this directory,
        // there are metadata files:
        // `desc`: package metadata
        // `files`: file list
        // `mtree` files and file metadata such as owner, link target, hash value (possibly compressed)
        // Example:
        //  $ ls /var/lib/pacman/local/just-1.46.0-1
        //  desc  files  mtree
        for local_db_entry in local_db.entries()? {
            let local_db_entry = local_db_entry?;
            if local_db_entry.file_type()?.is_dir() {
                let package_dir = local_db_entry.open_dir()?;
                let (desc, files) =
                    Self::package_info_from_dir(&package_dir).with_context(|| {
                        format!(
                            "parsing metadata of package {:?}",
                            local_db_entry.file_name()
                        )
                    })?;
                let basename = desc.base()?;
                let builddate = desc.builddate()?;
                let (component_id, _) = components.insert_full(basename.to_string(), builddate);
                Self::files_to_map(
                    &mut path_to_components,
                    ComponentId(component_id),
                    files.files(),
                    image_files,
                )?;
            }
        }
        Ok(Self {
            components,
            path_to_components,
        })
    }

    /// Open a directory corresponding to a package and expect it to contain relevant metadata
    /// in `desc` and `files` files.
    ///
    /// Returns two [`LocalAlpmDb`]: First for the parsed `desc` file, second for the parsed `files` file.
    fn package_info_from_dir(package_dir: &Dir) -> Result<(LocalAlpmDbFile, LocalAlpmDbFile)> {
        let desc = {
            let mut file = package_dir.open(DESC_FILENAME)?.into_std();
            let mut content = String::new();
            file.read_to_string(&mut content)
                .context("read desc file")?;
            content.parse::<LocalAlpmDbFile>()?
        };
        let files = {
            let mut file = package_dir.open(FILES_FILENAME)?.into_std();
            let mut content = String::new();
            file.read_to_string(&mut content)?;
            content.parse::<LocalAlpmDbFile>()?
        };
        Ok((desc, files))
    }

    fn files_to_map(
        path_to_components: &mut HashMap<Utf8PathBuf, Vec<ComponentId>>,
        component_id: ComponentId,
        pkgdb_files: Vec<&Utf8Path>,
        // TODO: Use this for path canonicalization
        _image_files: &FileMap,
    ) -> Result<()> {
        for path in pkgdb_files {
            // Unfortunately, we cannot differentiate between file types, because we only have paths.
            // As such, we will not use that information.
            // If it is needed in the future, the parser would have to be extended to read `mtree` files.
            // If only a directory/non-directory switch is needed, one could also check the paths themselves,
            // because directories consistently have a trailing '/' in their paths (this is also mandated by the spec).

            // let file_type = ...

            // The `files` file contains relative paths like "usr/bin/sh" (as it is mandated by the spec),
            // while canonicalization wants absolute paths.
            // Check that this is true just to be safe:
            if path.is_absolute() {
                bail!("{path} is absolute, while the ALPM specification mandates relative paths");
            }

            // SAFETY: "/" is always a valid path
            let mut absolute_path = Utf8PathBuf::from_str("/").unwrap();
            absolute_path.push(path);

            // TODO: Canonicalization using `absolute_path`

            path_to_components
                .entry(absolute_path)
                .or_default()
                .push(component_id);
        }
        Ok(())
    }
}

impl ComponentsRepo for AlpmComponentsRepo {
    fn name(&self) -> &'static str {
        REPO_NAME
    }

    fn default_priority(&self) -> usize {
        10
    }

    fn claims_for_path(&self, path: &Utf8Path, _file_type: FileType) -> Vec<ComponentId> {
        self.path_to_components
            .get(path)
            .map(|components| components.to_vec())
            .unwrap_or_default()
    }

    fn component_info(&self, id: ComponentId) -> ComponentInfo<'_> {
        // Safety: We handed out the ComponentId by ourselves and obtained it directly from the `IndexMap`
        let (pkgbase, build_time) = self.components.get_index(id.0).unwrap();
        ComponentInfo {
            name: pkgbase.as_str(),
            mtime_clamp: *build_time,
            stability: 0.0,
        }
    }
}

/// Parses file contents of ALPM local database files, i.e. `desc` and `files`.
/// Implements the [`FromStr`] trait, construct it by using `.parse()` on a &str.
///
/// cf. https://alpm.archlinux.page/specifications/alpm-db-desc.5.html
/// and https://alpm.archlinux.page/specifications/alpm-db-files.5.html
#[derive(Debug)]
pub struct LocalAlpmDbFile(HashMap<String, Vec<String>>);

impl FromStr for LocalAlpmDbFile {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let mut entries: HashMap<String, Vec<String>> = HashMap::new();
        let mut contents = None;
        for line in s.lines() {
            let new_header = line
                .strip_prefix('%')
                .and_then(|part| part.strip_suffix('%'));
            if let Some(new_header) = new_header
                && !new_header.is_empty()
            {
                if entries.contains_key(new_header) {
                    bail!("Duplicate headers");
                }
                contents = Some(entries.entry(new_header.to_string()).or_default());
            } else {
                // If contents is `None`, this means that we saw a content line without ever having seen
                // a header line before. This is not allowed, return an Error.
                contents
                    .as_mut()
                    .ok_or_else(|| anyhow!("File must start with a valid header"))?
                    .push(line.to_string());
            }
        }

        // The spec says: "Empty lines between sections are ignored."
        // So: Remove trailing empty lines.
        for value in entries.values_mut() {
            while let Some(entry) = value.last()
                && entry.is_empty()
            {
                // SAFETY: The loop condition ensures that there is a last entry that can be pop'd.
                value.pop().expect("value is empty");
            }
        }

        Ok(Self(entries))
    }
}

impl LocalAlpmDbFile {
    /// Returns the contents of the `key` entry.
    /// Returns an error if the entry contains more than a single line of content, while ignoring empty lines.
    ///
    /// The spec is a bit different for `alpm-db-desc` and `alpm-db-files`:
    /// The former says "Empty lines between sections are ignored" while the latter specifies:
    /// "Empty lines are ignored". This function uses the more permissive approach of the latter and ignores all empty lines.
    ///
    /// cf. https://alpm.archlinux.page/specifications/alpm-db-desc.5.html
    /// and https://alpm.archlinux.page/specifications/alpm-db-files.5.html
    pub fn get_single_line_value(&self, key: &str) -> Result<&str> {
        let lines = self.0.get(key).ok_or_else(|| anyhow!("key not found"))?;

        let mut non_empty = lines.iter().filter(|l| !l.is_empty());

        let first = non_empty.next().ok_or_else(|| anyhow!("no value found"))?;

        if non_empty.next().is_some() {
            bail!("unexpected extra data");
        }

        Ok(first)
    }

    /// Returns all lines of the `key` entry.
    /// Returns `None` if the attribute isn't present in the alpm file.
    pub fn get_multi_line_value(&self, key: &str) -> Option<&[String]> {
        self.0.get(key).map(|value| value.as_slice())
    }

    /// Gets the value of the %BUILDDATE% attribute of a `desc` file, if it is present and well-formed.
    /// Returns an error if the attribute isn't present in the `desc` file, if it is a multi-line string or cannot be parsed into an [`u64`].
    pub fn builddate(&self) -> Result<u64> {
        self.get_single_line_value("BUILDDATE")?
            .trim()
            .parse()
            .map_err(anyhow::Error::new)
    }

    /// Gets the value of the %BASE% attribute of a `desc` file, if it is present and well-formed.
    /// Returns an error if the attribute isn't present in the `desc` file or if it is a multi-line string.
    pub fn base(&self) -> Result<&str> {
        self.get_single_line_value("BASE")
    }

    /// Parses the %FILES% section of the `files` file and returns their contents.
    ///
    /// Note that even valid `files` may not have a %FILES% section according to the spec (https://alpm.archlinux.page/specifications/alpm-db-files.5.html):
    /// "Note, that if a package tracks no files (e.g. alpm-meta-package), then none of the following sections are present, and the alpm-db-files file is empty."
    pub fn files(&self) -> Vec<&Utf8Path> {
        self.get_multi_line_value("FILES")
            .map(|all_files| {
                all_files
                    .iter()
                    .map(|line| Utf8Path::new(line.as_str()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::Path};

    use camino::Utf8Path;
    use cap_std_ext::cap_std::{ambient_authority, fs::Dir};

    use crate::components::{
        ComponentsRepo, FileType,
        alpm::{AlpmComponentsRepo, LocalAlpmDbFile},
    };

    pub const DESC_CONTENTS: &str = r#"%NAME%
filesystem

%VERSION%
2025.10.12-1

%BASE%
filesystem

%DESC%
Base Arch Linux files

%URL%
https://archlinux.org

%ARCH%
any

%BUILDDATE%
1760286101

%INSTALLDATE%
1770909753

%PACKAGER%
David Runge <dvzrv@archlinux.org>

%SIZE%
24551

%LICENSE%
0BSD

%VALIDATION%
pgp

%DEPENDS%
iana-etc

%XDATA%
pkgtype=pkg
"#;

    pub const FILES_CONTENT: &str = r#"%FILES%
etc/
etc/protocols
etc/services
usr/
usr/share/
usr/share/iana-etc/
usr/share/iana-etc/port-numbers.iana
usr/share/iana-etc/protocol-numbers.iana
usr/share/licenses/
usr/share/licenses/iana-etc/
usr/share/licenses/iana-etc/LICENSE

%BACKUP%
etc/protocols	b9833a5373ef2f5df416f4f71ccb42eb
etc/services	b80b33810d79289b09bac307a99b4b54
"#;

    fn rootfs() -> Dir {
        let rootfs = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("arch-rootfs");
        Dir::open_ambient_dir(&rootfs, ambient_authority()).unwrap()
    }

    #[test]
    fn claims_correct_files() {
        let files = BTreeMap::new();
        let alpm = AlpmComponentsRepo::load(&rootfs(), &files)
            .unwrap()
            .unwrap();
        let claims = alpm.claims_for_path(Utf8Path::new("/usr"), FileType::Directory);
        assert!(claims.len() > 1);

        let claims = alpm.claims_for_path(Utf8Path::new("/etc/fstab"), FileType::File);
        assert_eq!(claims.len(), 1);
    }

    #[test]
    fn test_parse_desc() {
        let parsed_desc = DESC_CONTENTS.parse::<LocalAlpmDbFile>().unwrap();
        println!("{:#?}", parsed_desc);
        let base = parsed_desc.base().unwrap();
        let builddate = parsed_desc.builddate().unwrap();
        println!("base=[{base}], builddate=[{builddate}]");
    }

    #[test]
    fn test_parse_files() {
        let parsed_files = FILES_CONTENT.parse::<LocalAlpmDbFile>().unwrap();
        let mut as_paths = parsed_files.files().into_iter();

        assert_eq!(as_paths.next().unwrap(), Utf8Path::new("etc/"));
        assert_eq!(as_paths.next().unwrap(), Utf8Path::new("etc/protocols"));
        assert_eq!(as_paths.next().unwrap(), Utf8Path::new("etc/services"));
        assert_eq!(as_paths.next().unwrap(), Utf8Path::new("usr/"));
        assert_eq!(as_paths.next().unwrap(), Utf8Path::new("usr/share/"));
        assert_eq!(
            as_paths.next().unwrap(),
            Utf8Path::new("usr/share/iana-etc/")
        );
        assert_eq!(
            as_paths.next().unwrap(),
            Utf8Path::new("usr/share/iana-etc/port-numbers.iana")
        );
        assert_eq!(
            as_paths.next().unwrap(),
            Utf8Path::new("usr/share/iana-etc/protocol-numbers.iana")
        );
        assert_eq!(
            as_paths.next().unwrap(),
            Utf8Path::new("usr/share/licenses/")
        );
        assert_eq!(
            as_paths.next().unwrap(),
            Utf8Path::new("usr/share/licenses/iana-etc/")
        );
        assert_eq!(
            as_paths.next().unwrap(),
            Utf8Path::new("usr/share/licenses/iana-etc/LICENSE")
        );
        assert_eq!(as_paths.next(), None);
    }
}
