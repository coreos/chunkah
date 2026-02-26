use anyhow::{Context, Result, anyhow, bail};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::cap_std::fs::Dir;
use indexmap::IndexMap;
use std::{collections::HashMap, io::Read, os::unix::fs::MetadataExt, str::FromStr};

use crate::{
    components::{ComponentId, ComponentInfo, ComponentsRepo, FileMap, FileType},
    utils::{calculate_stability, canonicalize_parent_path},
};

const REPO_NAME: &str = "alpm";

/// These paths are searched for a local ALPM database. First is the default path of Arch Linux,
/// second is currently used by the popular ghcr.io/bootcrew/arch-bootc image.
const LOCALDB_PATHS: &[&str] = &["var/lib/pacman/local", "usr/lib/sysimage/lib/pacman/local"];

/// Filename of the ALPM `desc` database file that contains metadata about an installed package
const FILENAME_DESC: &str = "desc";
/// Filename of the ALPM `files` database file that contains a list of files contained in a package
const FILENAME_FILES: &str = "files";

/// Section name for the BASE package identifier
const SECTION_IDENTIFIER_BASE: &str = "BASE";
/// Section name for the BUILDDATE package build date
const SECTION_IDENTIFIER_BUILDDATE: &str = "BUILDDATE";
/// Section name for the FILES section, that contains all paths associated with the package
const SECTION_IDENTIFIER_FILES: &str = "FILES";

/// ALPM files read by the parser may not exceed `ALPM_DBFILE_MAXIMUM_SIZE` bytes. This should be plenty (64 MiB).
const ALPM_DBFILE_MAXIMUM_SIZE: u64 = 64 * 1024 * 1024;

pub struct AlpmComponentsRepo {
    /// Unique component (BASE) names mapped to builddate and stability, indexed by ComponentId.
    components: IndexMap<String, (u64, f64)>,

    /// Mapping from path to list of ComponentId.
    ///
    /// It's common for directories to be owned by more than one component (i.e.
    /// from _different_ packages).
    path_to_components: HashMap<Utf8PathBuf, Vec<ComponentId>>,
}

impl AlpmComponentsRepo {
    /// Locate, parse and index a local ALPM database in `rootfs` using common paths from [`LOCALDB_PATHS`]
    pub fn load(rootfs: &Dir, files: &FileMap, now: u64) -> Result<Option<Self>> {
        let local_db = LOCALDB_PATHS
            .iter()
            .find_map(|path| rootfs.open_dir(path).ok());
        let local_db = match local_db {
            Some(dir) => dir,
            None => return Ok(None),
        };
        Self::load_from_db(rootfs, &local_db, files, now).map(Some)
    }

    /// Starting from the `local_db` base directory, iterate over the packages in the local database,
    /// process package metadata and generate an index of components and their files.
    pub fn load_from_db(
        rootfs: &Dir,
        local_db: &Dir,
        image_files: &FileMap,
        now: u64,
    ) -> Result<Self> {
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
                let stability = calculate_stability(&[], builddate, now)?;
                let components_entry = components.entry(basename.to_string());
                let component_id = ComponentId(components_entry.index());
                match components_entry {
                    indexmap::map::Entry::Occupied(mut e) => {
                        // A package built from the same %BASE% was already added:
                        // (1) We want the most current (max) builddate as the clamp value
                        // (2) We want the lowest stability score (min), as a layer can only be
                        //     as stable as the most unstable part.
                        let e: &mut (u64, f64) = e.get_mut();
                        e.0 = e.0.max(builddate);
                        e.1 = e.1.min(stability);
                    }
                    indexmap::map::Entry::Vacant(e) => {
                        // Package with same value for %BASE% did not exist before, so we add it
                        e.insert((builddate, stability));
                    }
                }
                Self::files_to_map(
                    &mut path_to_components,
                    component_id,
                    files.files(),
                    image_files,
                    rootfs,
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
        // We read two files: desc and files. Both are read and parsed in the same way.
        let read_dbfile = |filename| {
            let mut database_file = package_dir.open(filename)?.into_std();

            // Make sure that the file is not too large to read it in memory.
            let size = database_file.metadata()?.size();
            if size > ALPM_DBFILE_MAXIMUM_SIZE {
                bail!(
                    "file is too large: {filename} (size: {size}, maximum: {ALPM_DBFILE_MAXIMUM_SIZE})"
                );
            }

            let mut content = String::with_capacity(
                // SAFETY: We know that size is less than `ALPM_DBFILE_MAXIMUM_SIZE` and
                // as such small enough to fit into an `usize` on every reasonable platform.
                usize::try_from(size).expect("file size value too large for usize"),
            );
            database_file.read_to_string(&mut content)?;

            // Finally parse the file
            content.parse::<LocalAlpmDbFile>()
        };
        let desc = read_dbfile(FILENAME_DESC).context("read and parse desc")?;
        let files = read_dbfile(FILENAME_FILES).context("read and parse files")?;
        Ok((desc, files))
    }

    /// Associates the given `component_id` with all canonicalized paths of the package given
    /// in `pkgdb_files` in `path_to_components`
    fn files_to_map(
        path_to_components: &mut HashMap<Utf8PathBuf, Vec<ComponentId>>,
        component_id: ComponentId,
        pkgdb_files: Vec<&Utf8Path>,
        image_files: &FileMap,
        rootfs: &Dir,
    ) -> Result<()> {
        let mut canonicalization_cache = HashMap::new();
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

            let mut absolute_path = Utf8PathBuf::from("/");
            absolute_path.push(path);

            let canonical_path = canonicalize_parent_path(
                rootfs,
                image_files,
                &absolute_path,
                &mut canonicalization_cache,
            )?;

            path_to_components
                .entry(canonical_path)
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
        let (pkgbase, (builddate, stability)) = self
            .components
            .get_index(id.0)
            // SAFETY: We handed out the ComponentId by ourselves and obtained it directly from the `IndexMap`
            .expect("invalid ComponentId");
        ComponentInfo {
            name: pkgbase.as_str(),
            mtime_clamp: *builddate,
            stability: *stability,
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

        // "An alpm-db-desc file is a UTF-8 encoded, newline-delimited file consisting of a series of sections." (`alpm-db-desc` spec)
        // As such, we split by lines and try to find the sections and their contents:
        for line in s.lines() {
            let new_header = Self::match_valid_header(line);
            if let Some(new_header) = new_header {
                // We do not allow for the same section name to appear twice.
                if entries.contains_key(new_header) {
                    bail!("Duplicate section: {new_header}");
                }
                // We have found a new section and initialize it in our `entries` map.
                // In order to save on the map lookup for each line, we keep a mutable reference
                // to the contents of the current section.
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
    /// Returns an error if the entry contains more than a single line of content.
    ///
    /// The spec is different for `alpm-db-desc` and `alpm-db-files`:
    /// The former says "Empty lines between sections are ignored" while the latter specifies:
    /// "Empty lines are ignored". This function uses the more restrictive approach of the first and will _not_
    /// filter leading newlines. This means single-value sections must not have any newlines after their section headers.
    ///
    /// cf. https://alpm.archlinux.page/specifications/alpm-db-desc.5.html
    /// and https://alpm.archlinux.page/specifications/alpm-db-files.5.html
    pub fn get_single_line_value(&self, section: &str) -> Result<&str> {
        let mut lines = self
            .0
            .get(section)
            .ok_or_else(|| anyhow!("section not found: {section}"))?
            .iter()
            .map(|line| line.as_str());
        let first = lines
            .next()
            .ok_or_else(|| anyhow!("no value found for section {section}"))?;

        if lines.next().is_some() {
            bail!("unexpected extra data in section {section}");
        }

        Ok(first)
    }

    /// Returns all lines of the `key` entry.
    /// Returns `None` if the attribute isn't present in the alpm file.
    ///
    /// Note that the spec is different for `alpm-db-desc` and `alpm-db-files` (see [`Self::get_single_line_value`]).
    /// If you are parsing a `alpm-db-files` file, you might need to filter additional newlines by yourself.
    /// The function [`Self::files`] already does this for the '%FILES%' section.
    pub fn get_multi_line_value(&self, section: &str) -> Option<&[String]> {
        self.0.get(section).map(|value| value.as_slice())
    }

    /// Gets the value of the %BUILDDATE% attribute of a `desc` file, if it is present and well-formed.
    /// Returns an error if the attribute isn't present in the `desc` file, if it is a multi-line string or cannot be parsed into an [`u64`].
    pub fn builddate(&self) -> Result<u64> {
        self.get_single_line_value(SECTION_IDENTIFIER_BUILDDATE)?
            .trim()
            .parse()
            .map_err(anyhow::Error::new)
    }

    /// Gets the value of the %BASE% attribute of a `desc` file, if it is present and well-formed.
    /// Returns an error if the attribute isn't present in the `desc` file or if it is a multi-line string.
    pub fn base(&self) -> Result<&str> {
        self.get_single_line_value(SECTION_IDENTIFIER_BASE)
    }

    /// Parses the %FILES% section of the `files` file and returns their contents.
    ///
    /// Empty lines will be ignored as to the `alpm-db-files` specification.
    ///
    /// Note that even valid `files` may not have a %FILES% section according to the spec (https://alpm.archlinux.page/specifications/alpm-db-files.5.html):
    /// "Note, that if a package tracks no files (e.g. alpm-meta-package), then none of the following sections are present, and the alpm-db-files file is empty."
    pub fn files(&self) -> Vec<&Utf8Path> {
        self.get_multi_line_value(SECTION_IDENTIFIER_FILES)
            .map(|all_files| {
                all_files
                    .iter()
                    .filter(|line| !line.is_empty())
                    .map(|line| Utf8Path::new(line.as_str()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Checks that the given line is a well-formed header line and returns the section name if it is
    ///
    /// "Each section header line contains the section name in all capital letters, surrounded by percent signs (e.g. %NAME%)."
    /// cf. https://alpm.archlinux.page/specifications/alpm-db-desc.5.html
    fn match_valid_header(line: &str) -> Option<&str> {
        let maybe_valid = line
            // Line needs to start and end with a '%' character
            .strip_prefix('%')
            .and_then(|line| line.strip_suffix('%'));
        if let Some(line) = maybe_valid {
            // We know our line starts and ends with '%'.
            // In addition: The name must be at least one character long and all characters
            // need to be ASCII uppercase characters.
            let contains_section_name = !line.is_empty();
            let is_well_formed = line.chars().all(|c| c.is_ascii_uppercase());
            if contains_section_name && is_well_formed {
                return Some(line);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::Path,
    };

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

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn claims_correct_files() {
        let files = BTreeMap::new();
        let alpm = AlpmComponentsRepo::load(&rootfs(), &files, now_secs())
            .unwrap()
            .unwrap();
        let claims = alpm.claims_for_path(Utf8Path::new("/usr"), FileType::Directory);
        assert_eq!(claims.len(), 2);
        let mut component_info = claims.iter().map(|claim| alpm.component_info(*claim));
        let mut expected_components = ["iana-etc", "filesystem"]
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert!(expected_components.remove(component_info.next().unwrap().name));
        assert!(expected_components.remove(component_info.next().unwrap().name));
        assert!(expected_components.is_empty());
        assert!(component_info.next().is_none());

        let claims = alpm.claims_for_path(Utf8Path::new("/etc/fstab"), FileType::File);
        assert_eq!(claims.len(), 1);
        let mut component_info = claims.iter().map(|claim| alpm.component_info(*claim));
        assert_eq!(component_info.next().unwrap().name, "filesystem");
        assert!(component_info.next().is_none());
    }

    #[test]
    fn test_parse_desc() {
        let parsed_desc = DESC_CONTENTS.parse::<LocalAlpmDbFile>().unwrap();
        assert_eq!(parsed_desc.base().unwrap(), "filesystem");
        assert_eq!(parsed_desc.builddate().unwrap(), 1760286101);
        assert_eq!(
            parsed_desc.get_single_line_value("NAME").unwrap(),
            "filesystem"
        );
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

        let mut other_section = parsed_files
            .get_multi_line_value("BACKUP")
            .unwrap()
            .into_iter();
        assert_eq!(
            other_section.next().unwrap(),
            "etc/protocols\tb9833a5373ef2f5df416f4f71ccb42eb"
        );
        assert_eq!(
            other_section.next().unwrap(),
            "etc/services\tb80b33810d79289b09bac307a99b4b54"
        );
        assert_eq!(other_section.next(), None);
    }
}
