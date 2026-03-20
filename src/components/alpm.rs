use std::{collections::HashMap, str::FromStr};

use anyhow::{Context, Result, anyhow, bail};
use camino::{Utf8Path, Utf8PathBuf};
use cap_std_ext::cap_std::fs::Dir;
use indexmap::IndexMap;
use ocidir::cap_std::fs::{FileType, ReadDir};

use crate::{
    components::{ComponentId, ComponentInfo, ComponentsRepo, FileMap},
    utils::{calculate_stability, canonicalize_parent_path, read_file_contents_to_string_checked},
};

const REPO_NAME: &str = "alpm";

/// This is the default path to the pacman configuration file.
/// Before trying the `LOCALDB_PATHS` below, we'll try to parse it in order to
/// see if a custom `DBPath` was specified and use whatever is configured there.
const PACMAN_CONF_PATH: &str = "etc/pacman.conf";

/// The key that describes the path to the local package database inside `pacman.conf`
const PACMAN_CONF_DB_PATH_KEY: &str = "DBPath";

/// Default path for the local package database
const LOCALDB_DEFAULT_PATH: &str = "var/lib/pacman/local";

/// Every local alpm database should have this file. It contains the database version.
const LOCALDB_VERSION_FILE: &str = "ALPM_DB_VERSION";

/// Most recent version this parser was tested with
const LOCALDB_TESTED_VERSION: u32 = 9;

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

/// ALPM files read by the parser may not exceed `ALPM_FILE_MAXIMUM_SIZE` bytes. This should be plenty (64 MiB).
const ALPM_FILE_MAXIMUM_SIZE: u64 = 64 * 1024 * 1024;

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
    /// Locate, parse and index a local ALPM database in `rootfs` using a package database specified
    /// in pacman.conf or from common paths as a fallback.
    pub fn load(rootfs: &Dir, files: &FileMap, now: u64) -> Result<Option<Self>> {
        match Self::try_locate_local_db(rootfs).context("locating local alpm database")? {
            Some(local_db) => Self::load_from_local_db(rootfs, &local_db, files, now)
                .context("reading alpm local database contents")
                .map(Some),
            None => {
                tracing::debug!("could not locate any local alpm database");
                Ok(None)
            }
        }
    }

    /// Starting from the `local_db` base directory, iterate over the packages in the local database,
    /// process package metadata and generate an index of components and their files.
    pub fn load_from_local_db(
        rootfs: &Dir,
        local_db: &Dir,
        rootfs_files: &FileMap,
        now: u64,
    ) -> Result<Self> {
        let mut components = IndexMap::new();
        let mut path_to_components = HashMap::new();
        let mut canonicalization_cache = HashMap::new();
        let mut package_count: usize = 0;

        // The local package database is basically a directory that contains
        // one directory for each locally installed package. Inside this directory,
        // there are metadata files:
        // `desc`: package metadata
        // `files`: file list
        // `mtree` files and file metadata such as owner, link target, hash value (possibly compressed)
        // Example:
        //  $ ls /var/lib/pacman/local/just-1.46.0-1
        //  desc  files  mtree
        let db_entries = LocalAlpmDbIterator::new(local_db)
            .context("opening local alpm database for iteration")?;
        for local_db_entry in db_entries {
            let local_db_entry =
                local_db_entry.context("getting next entry for local alpm database")?;
            let basename = local_db_entry.desc.base().with_context(|| {
                format!(
                    "parsing base from desc file of alpm db entry {}",
                    &local_db_entry.source
                )
            })?;
            let builddate = local_db_entry.desc.builddate().with_context(|| {
                format!(
                    "parsing builddate from desc file of alpm db entry {}",
                    &local_db_entry.source
                )
            })?;
            let stability = calculate_stability(&[], builddate, now);
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
                    tracing::trace!(component = %basename, builddate = %e.0, stability = %e.1, "multiple alpm components with same basename");
                }
                indexmap::map::Entry::Vacant(e) => {
                    // Package with same value for %BASE% did not exist before, so we add it
                    e.insert((builddate, stability));
                    tracing::trace!(component = %basename, id = component_id.0, "alpm component created");
                }
            }
            tracing::debug!(component = %basename, "build file map for alpm component");
            Self::files_to_map(
                &mut path_to_components,
                component_id,
                local_db_entry.files.files(),
                rootfs_files,
                rootfs,
                &mut canonicalization_cache,
            )
            .with_context(|| {
                format!(
                    "adding package {} to map from paths to alpm components",
                    &local_db_entry.source
                )
            })?;
            package_count += 1;
        }

        tracing::debug!(
            packages = package_count,
            components = components.len(),
            paths = path_to_components.len(),
            "loaded alpm database"
        );
        Ok(Self {
            components,
            path_to_components,
        })
    }

    /// Associates the given `component_id` in `path_to_components` with all canonicalized paths of the package given
    /// in `pkgdb_files`.
    fn files_to_map(
        path_to_components: &mut HashMap<Utf8PathBuf, Vec<ComponentId>>,
        component_id: ComponentId,
        pkgdb_files: Vec<&Utf8Path>,
        image_files: &FileMap,
        rootfs: &Dir,
        canonicalization_cache: &mut HashMap<Utf8PathBuf, Utf8PathBuf>,
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
                bail!("the ALPM specification mandates relative paths, but {path} is absolute");
            }

            let mut absolute_path = Utf8PathBuf::from("/");
            absolute_path.push(path);

            let canonical_path = canonicalize_parent_path(
                rootfs,
                image_files,
                &absolute_path,
                canonicalization_cache,
            )?;
            if canonical_path != absolute_path {
                tracing::trace!(original = %absolute_path, canonical = %canonical_path, "path canonicalized");
            }

            path_to_components
                .entry(canonical_path)
                .or_default()
                .push(component_id);
        }
        Ok(())
    }

    /// Try to locate the local alpm database directory and return a handle to it.
    /// If we find a pacman.conf file, we will use the `DBPath` specified therein,
    /// or fall back to [`LOCALDB_DEFAULT_PATH`].
    /// If no pacman.conf exists or it doesn't contain a `DBPath`, or if the default directory doesn't exist,
    /// we return `Ok(None)`. If any of these _do_ exist, an `Err` is returned if pacman.conf/the local database
    /// can't be read for any reason.
    fn try_locate_local_db(rootfs: &Dir) -> Result<Option<Dir>> {
        match Self::open_pacman_conf_read_dbpath(rootfs)
            .context("reading DBPath from pacman.conf")?
        {
            Some(mut dbpath) => {
                // We found a valid `DBPath` directive in the pacman.conf file, which the user wants us to use,
                // so we won't fall back to the default path anymore.
                // Now, `dbpath` is the relative path to the alpm database directory, but what we're looking for is the _local_ db
                dbpath.push("local");
                Self::validate_and_open_alpm_db(rootfs, &dbpath)
            }
            None => Self::validate_and_open_alpm_db(rootfs, Utf8Path::new(LOCALDB_DEFAULT_PATH)),
        }
    }

    /// Tries to open the given `dbpath` for reading and locates the version file.
    /// If there is a valid version file inside `dbpath`, the directory handle is returned.
    /// Prints a warning if a valid `dbpath` is found, but the local database version number
    /// does not match the one the code was tested with.
    fn validate_and_open_alpm_db(rootfs: &Dir, dbpath: &Utf8Path) -> Result<Option<Dir>> {
        // This is fine: If the `dbpath` doesn't exist, there likely is some other database path
        // which is tried later on, or this isn't an Arch Linux system at all.
        if !rootfs.exists(dbpath) {
            return Ok(None);
        }

        // If the path does exist, this probably _is_ an Arch Linux system and the user
        // expects us to read the database. If this doesn't work, this is actually an error.
        let localdb_candidate_dir = rootfs
            .open_dir(dbpath)
            .with_context(|| format!("opening {dbpath} as a local alpm database"))?;
        let mut version_file = localdb_candidate_dir
            .open(LOCALDB_VERSION_FILE)
            .with_context(|| format!("opening {dbpath}/{LOCALDB_VERSION_FILE}"))?;
        let version: u32 = {
            // version file should contain a single line with the version number, followed by a newline.
            // Allow for an extra byte if the version number increases to 2 digits.
            let version_file_contents = read_file_contents_to_string_checked(&mut version_file, 3)
                .with_context(|| format!("reading {dbpath}/{LOCALDB_VERSION_FILE}"))?;
            version_file_contents
                .trim()
                .parse()
                .context("parsing alpm local database version file contents")?
        };
        if version != LOCALDB_TESTED_VERSION {
            tracing::warn!(database_version = %version, tested = %LOCALDB_TESTED_VERSION, "alpm local database version has not been tested");
        }
        Ok(Some(localdb_candidate_dir))
    }

    /// Try to open the `pacman.conf` configuration file and extract the `DBPath`.
    ///
    /// Returns an error if pacman.conf cannot be read.
    /// Returns the relative path of the local package database derived from the configured `DBPath` on success.
    /// Returns `Ok(None)` if the `rootfs` does not contain a `pacman.conf` file or it does not specify a `DBPath`.
    fn open_pacman_conf_read_dbpath(rootfs: &Dir) -> Result<Option<Utf8PathBuf>> {
        // Don't return an error if no pacman.conf exists - we can just fall back to the default value later on
        if !rootfs.exists(PACMAN_CONF_PATH) {
            return Ok(None);
        }

        let contents = {
            // If a pacman.conf exists, it might contain a `DBPath` directive which the user wants us to consider.
            // As such, _do_ return an error if the file exists but we can't read it for some reason.
            let mut pacman_conf_file = rootfs
                .open(PACMAN_CONF_PATH)
                .context("opening pacman.conf")?;
            read_file_contents_to_string_checked(&mut pacman_conf_file, ALPM_FILE_MAXIMUM_SIZE)
                .context("reading pacman.conf")?
        };

        Ok(Self::parse_pacman_conf_dbpath(&contents))
    }

    /// Tries to parse and extract the contents of the `DBPath` key from the contents of a `pacman.conf`.
    /// If multiple valid `DBPath` directives exist, the last one wins.
    ///
    /// Returns the relative path of the local package database derived from the configured `DBPath` on success.
    /// If no `DBPath` can be found at a valid location (i.e. uncommented inside an "[options]" section), return `None`
    fn parse_pacman_conf_dbpath(pacman_conf: &str) -> Option<Utf8PathBuf> {
        let mut in_options = false;
        let mut result = None;

        for (idx, raw_line) in pacman_conf.lines().enumerate() {
            // Handle (inline) comments
            let line = raw_line.split('#').next().unwrap().trim();
            if line.is_empty() {
                continue;
            }

            // Detect sections
            if line.starts_with('[') && line.ends_with(']') {
                in_options = line == "[options]";
                continue;
            }

            // The key we're looking for only occurs in the [options] section, so we can skip the rest of the lines
            if !in_options {
                continue;
            }

            let (key, value) = match line.split_once('=') {
                Some(kv) => kv,
                None => continue,
            };

            if key.trim() != PACMAN_CONF_DB_PATH_KEY {
                continue;
            }

            let path = Utf8Path::new(value.trim());

            // Usually, the path in `pacman.conf` is supposed to be absolute, but we want a relative path instead
            let path = path.strip_prefix("/").unwrap_or(path);

            if path.as_str().is_empty() {
                tracing::warn!(line = idx + 1, "ignoring empty DBPath value in pacman.conf");
                continue;
            }

            // Don't stop parsing here: The last DBPath wins
            result = Some(path.to_path_buf());
        }

        result
    }
}

impl ComponentsRepo for AlpmComponentsRepo {
    fn name(&self) -> &'static str {
        REPO_NAME
    }

    fn default_priority(&self) -> usize {
        10
    }

    fn strong_claims_for_path(
        &self,
        path: &Utf8Path,
        _file_info: &super::FileInfo,
    ) -> Vec<ComponentId> {
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

struct LocalAlpmDbIterator {
    local_db_entries: ReadDir,
}

impl LocalAlpmDbIterator {
    fn new(local_db_dir: &Dir) -> Result<Self> {
        let local_db_entries = local_db_dir
            .entries()
            .context("reading local alpm database directory")?;
        Ok(Self { local_db_entries })
    }

    /// Find the next valid directory representing an installed package, try to parse
    /// the database files contained therein and return a [`LocalAlpmDbEntryComponents`]
    /// that can be used to retrieve the values of the relevant sections.
    ///
    /// Return `Ok(None)` if all directory entries were iterated over.
    fn next_item_as_result(&mut self) -> Result<Option<LocalAlpmDbEntryComponents>> {
        for local_db_entry in self.local_db_entries.by_ref() {
            let local_db_entry =
                local_db_entry.context("getting next entry for local alpm database")?;
            let local_db_entry_name = local_db_entry.file_name().display().to_string();

            // Installed packages are represented as directories, so determine the file type of
            // the current `DirEntry` and skip non-directories
            let local_db_entry_file_type = local_db_entry.file_type().with_context(|| {
                format!(
                    "determining file type of database entry {}",
                    &local_db_entry_name
                )
            })?;

            // Check if the current `DirEntry` is a directory that represents a package in the local database
            // Related: https://github.com/coreos/chunkah/issues/20
            // Some filesystems don't provide a useful file type so we need to call `stat`
            let local_db_entry_is_dir = if local_db_entry_file_type == FileType::unknown() {
                local_db_entry
                    .metadata()
                    .with_context(|| {
                        format!(
                            "getting metadata for database entry {}",
                            &local_db_entry_name
                        )
                    })?
                    .is_dir()
            } else {
                local_db_entry_file_type.is_dir()
            };
            if !local_db_entry_is_dir {
                continue;
            }

            let package_dir = local_db_entry.open_dir().with_context(|| {
                format!(
                    "opening dir for alpm database entry {}",
                    &local_db_entry_name
                )
            })?;
            return Self::package_info_from_dir(&package_dir, local_db_entry_name)
                .with_context(|| {
                    format!(
                        "parsing metadata of package {}",
                        local_db_entry.file_name().display()
                    )
                })
                .map(Some);
        }

        Ok(None)
    }

    /// Open a directory corresponding to a package and expect it to contain relevant metadata
    /// in `desc` and `files` files.
    ///
    /// Returns a [`LocalAlpmDbEntryComponents`] containing the parsing results
    fn package_info_from_dir(
        package_dir: &Dir,
        source: String,
    ) -> Result<LocalAlpmDbEntryComponents> {
        // We read two files: desc and files. Both are read and parsed in the same way.
        let read_dbfile = |filename| {
            let mut dbfile = package_dir
                .open(filename)
                .context("opening database file")?;
            let content = read_file_contents_to_string_checked(&mut dbfile, ALPM_FILE_MAXIMUM_SIZE)
                .context("reading database file")?;
            content
                .parse::<LocalAlpmDbFile>()
                .context("parsing database file")
        };
        let desc =
            LocalAlpmDbDescFile(read_dbfile(FILENAME_DESC).context("reading and parsing 'desc'")?);
        let files = LocalAlpmDbFilesFile(
            read_dbfile(FILENAME_FILES).context("reading and parsing 'files'")?,
        );
        Ok(LocalAlpmDbEntryComponents {
            source,
            desc,
            files,
        })
    }
}

impl Iterator for LocalAlpmDbIterator {
    type Item = Result<LocalAlpmDbEntryComponents>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_item_as_result().transpose()
    }
}

/// Contains parsed versions of important alpm database files and the name of the source directory
struct LocalAlpmDbEntryComponents {
    /// Include a human-readable version of the source directory,
    /// which is useful for constructing error messages if something goes wrong
    source: String,
    /// Parsed `desc` database file, which contains metadata like name or builddate
    desc: LocalAlpmDbDescFile,
    /// Parsed `files` database file, which can contain a list of paths owned by the package
    files: LocalAlpmDbFilesFile,
}

/// Parses file contents of ALPM local database files, i.e. `desc` and `files`.
/// Implements the [`FromStr`] trait, construct it by using `.parse()` on a &str.
///
/// Represents a generic database file. The sections can be queried using
/// [`LocalAlpmDbFile::get_single_line_value`] and [`LocalAlpmDbFile::get_multi_line_value`].
///
/// Can be wrapped in a [`LocalAlpmDbDescFile`] or [`LocalAlpmDbFilesFile`] for more specific accessors.
///
/// cf. https://alpm.archlinux.page/specifications/alpm-db-desc.5.html
/// and https://alpm.archlinux.page/specifications/alpm-db-files.5.html
#[derive(Debug)]
struct LocalAlpmDbFile(HashMap<String, Vec<String>>);

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
                    bail!("duplicate section: {new_header}");
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
                    .ok_or_else(|| anyhow!("file must start with a valid header"))?
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
    fn get_single_line_value(&self, section: &str) -> Result<&str> {
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
    fn get_multi_line_value(&self, section: &str) -> Option<&[String]> {
        self.0.get(section).map(|value| value.as_slice())
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

/// Wraps a generic [`LocalAlpmDbFile`] and offers accessor functions that are specific to `desc` files.
struct LocalAlpmDbDescFile(LocalAlpmDbFile);

impl LocalAlpmDbDescFile {
    /// Gets the value of the %BUILDDATE% attribute of a `desc` file, if it is present and well-formed.
    /// Returns an error if the attribute isn't present in the `desc` file, if it is a multi-line string or cannot be parsed into an [`u64`].
    fn builddate(&self) -> Result<u64> {
        self.0
            .get_single_line_value(SECTION_IDENTIFIER_BUILDDATE)?
            .trim()
            .parse()
            .context("parsing package builddate from desc file as an u64")
    }

    /// Gets the value of the %BASE% attribute of a `desc` file, if it is present and well-formed.
    /// Returns an error if the attribute isn't present in the `desc` file or if it is a multi-line string.
    fn base(&self) -> Result<&str> {
        self.0
            .get_single_line_value(SECTION_IDENTIFIER_BASE)
            .context("parsing package base section value from desc file")
    }
}

/// Wraps a generic [`LocalAlpmDbFile`] and offers accessor functions that are specific to `files` files.
struct LocalAlpmDbFilesFile(LocalAlpmDbFile);

impl LocalAlpmDbFilesFile {
    /// Parses the %FILES% section of the `files` file and returns their contents.
    ///
    /// Empty lines will be ignored as to the `alpm-db-files` specification.
    ///
    /// Note that even valid `files` may not have a %FILES% section according to the spec (https://alpm.archlinux.page/specifications/alpm-db-files.5.html):
    /// "Note, that if a package tracks no files (e.g. alpm-meta-package), then none of the following sections are present, and the alpm-db-files file is empty."
    fn files(&self) -> Vec<&Utf8Path> {
        self.0
            .get_multi_line_value(SECTION_IDENTIFIER_FILES)
            .map(|all_files| {
                all_files
                    .iter()
                    .filter(|line| !line.is_empty())
                    .map(|line| Utf8Path::new(line.as_str()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs::File,
        io::Read,
        path::Path,
    };

    use camino::{Utf8Path, Utf8PathBuf};
    use cap_std_ext::cap_std::{ambient_authority, fs::Dir};
    use tempfile::TempDir;

    use crate::components::{
        ComponentsRepo, FileInfo, FileType,
        alpm::{
            AlpmComponentsRepo, LocalAlpmDbDescFile, LocalAlpmDbFile, LocalAlpmDbFilesFile,
            PACMAN_CONF_PATH,
        },
    };

    fn fixture() -> (TempDir, Dir) {
        let pkgdb_archive = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("arch-pkgdb.tar.gz");
        let tempdir = tempfile::tempdir().unwrap();

        let mut pkgdb_archive = File::open(pkgdb_archive).unwrap();
        let mut archive = flate2::read::GzDecoder::new(&mut pkgdb_archive);
        let mut archive = tar::Archive::new(&mut archive);
        archive.unpack(tempdir.path()).unwrap();

        let dir = Dir::open_ambient_dir(tempdir.path(), ambient_authority()).unwrap();
        (tempdir, dir)
    }

    fn fixture_pacman_conf() -> String {
        let (_tempdir, dir) = fixture();
        let mut contents = String::new();
        dir.open(PACMAN_CONF_PATH)
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        contents
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn claims_correct_files() {
        pub const IMPORTANT_ARCH_LINUX_PACKAGES_IN_BASE: &[&str] = &[
            "systemd",
            "archlinux-keyring",
            "pacman",
            "binutils",
            "systemd",
            "kbd",
            "dbus",
            "cryptsetup",
            "lvm2",
            "libusb",
            "licenses",
            "gzip",
            "util-linux",
            "shadow",
            "kmod",
            "hwdata",
            "gettext",
            "curl",
            "ca-certificates",
            "tar",
            "sed",
            "procps-ng",
            "grep",
            "pcre2",
            "gawk",
            "findutils",
            "file",
            "bzip2",
            "coreutils",
            "pam",
            "zstd",
            "xz",
            "lz4",
            "openssl",
            "e2fsprogs",
            "util-linux",
            "sqlite",
            "zlib",
            "attr",
            "acl",
            "bash",
            "readline",
            "ncurses",
            "gcc",
            "glibc",
            "tzdata",
            "filesystem",
            "iana-etc",
        ];
        let (_tempdir, rootfs) = fixture();
        let files = BTreeMap::new();
        let alpm = AlpmComponentsRepo::load(&rootfs, &files, now_secs())
            .unwrap()
            .unwrap();

        // Many packages will claim `/usr` (129 packages at the time of writing this test)
        let claims = alpm
            .strong_claims_for_path(Utf8Path::new("/usr"), &FileInfo::dummy(FileType::Directory));
        assert!(claims.len() > 100);

        // Multiple packages in base have gcc as their basename, so in terms of component names, they will be seen multiple times
        let mut expected_components = IMPORTANT_ARCH_LINUX_PACKAGES_IN_BASE
            .into_iter()
            .map(|pkg| *pkg)
            .collect::<BTreeSet<_>>();
        let component_info = claims.iter().map(|claim| alpm.component_info(*claim));
        for component in component_info {
            let component_name = component.name;
            let _ = expected_components.remove(component_name);
        }
        assert!(
            expected_components.is_empty(),
            "all expected components should have been seen"
        );

        // `/etc/fstab` belongs to the `filesystem` package
        let claims = alpm.strong_claims_for_path(
            Utf8Path::new("/etc/fstab"),
            &FileInfo::dummy(FileType::File),
        );
        assert_eq!(claims.len(), 1);
        let mut component_info = claims.iter().map(|claim| alpm.component_info(*claim));
        assert_eq!(component_info.next().unwrap().name, "filesystem");
        assert!(component_info.next().is_none());
    }

    #[test]
    fn claims_correct_files_with_autodiscover() {
        let (_tempdir, rootfs) = fixture();
        // We don't want to use the DBPath from pacman.conf but try to find the DBPath by ourselves:
        rootfs.remove_file("etc/pacman.conf").unwrap();

        let files = BTreeMap::new();
        let alpm = AlpmComponentsRepo::load(&rootfs, &files, now_secs())
            .unwrap()
            .unwrap();

        // All packages claim `/usr`
        let claims = alpm
            .strong_claims_for_path(Utf8Path::new("/usr"), &FileInfo::dummy(FileType::Directory));
        assert!(claims.len() > 100);

        // Basic sanity checks when using an auto-discovered path seemed to work.
        // The actual claims are tested in `claims_correct_files` which will
        // use the database path from pacman.conf.
    }

    #[test]
    fn error_on_existing_but_invalid_local_database_dir() {
        let (_tempdir, rootfs) = fixture();
        rootfs
            .remove_file("var/lib/pacman/local/ALPM_DB_VERSION")
            .unwrap();
        let files = BTreeMap::new();
        let alpm = AlpmComponentsRepo::load(&rootfs, &files, now_secs());
        assert!(alpm.is_err());
    }

    #[test]
    fn will_not_error_on_non_archlinux() {
        let tempdir = tempfile::tempdir().unwrap();
        let dir = Dir::open_ambient_dir(tempdir.path(), ambient_authority()).unwrap();
        let alpm = AlpmComponentsRepo::load(&dir, &BTreeMap::new(), now_secs());
        assert!(alpm.unwrap().is_none());
    }

    #[test]
    fn test_parse_desc() {
        const DESC_CONTENTS: &str = r#"%NAME%
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
        let parsed_desc = DESC_CONTENTS.parse::<LocalAlpmDbFile>().unwrap();
        assert_eq!(
            parsed_desc.get_single_line_value("NAME").unwrap(),
            "filesystem",
            "generic parser can parse the NAME section"
        );
        let parsed_desc = LocalAlpmDbDescFile(parsed_desc);
        assert_eq!(parsed_desc.base().unwrap(), "filesystem");
        // This is the builddate at the time of writing the test.
        // Package will probably be newer if the fixture contents are regenerated at a later point in time
        assert!(parsed_desc.builddate().unwrap() >= 1760286101);
    }

    #[test]
    fn test_parse_files() {
        const FILES_CONTENT: &str = r#"%FILES%
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
        const EXPECTED_PATHS: &[&str] = &[
            "etc/",
            "etc/protocols",
            "etc/services",
            "usr/",
            "usr/share/",
            "usr/share/iana-etc/",
            "usr/share/iana-etc/port-numbers.iana",
            "usr/share/iana-etc/protocol-numbers.iana",
            "usr/share/licenses/",
            "usr/share/licenses/iana-etc/",
            "usr/share/licenses/iana-etc/LICENSE",
        ];
        let mut expected_paths_set = EXPECTED_PATHS
            .iter()
            .map(|path| Utf8Path::new(path))
            .collect::<BTreeSet<_>>();

        let parsed_files = FILES_CONTENT.parse::<LocalAlpmDbFile>().unwrap();

        // Test that the generic parser can parse other sections, such as BACKUP
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

        let parsed_files = LocalAlpmDbFilesFile(parsed_files);
        for path_from_files in parsed_files.files().into_iter() {
            assert!(
                expected_paths_set.remove(path_from_files),
                "path from files must be expected"
            );
        }
        assert_eq!(
            expected_paths_set.len(),
            0,
            "all expected paths must have been parsed"
        );
    }

    #[test]
    fn test_parse_empty_files() {
        assert!(
            "".parse::<LocalAlpmDbFile>().is_ok(),
            "files database files are explicitly permitted to be empty by the specification"
        );
    }

    #[test]
    fn pacman_conf_extract_dbpath_from_fixture() {
        let fixture = fixture_pacman_conf();
        let dbpath = AlpmComponentsRepo::parse_pacman_conf_dbpath(&fixture);
        assert!(dbpath.is_some(), "pacman.conf contains a valid DBPath");
        assert_eq!(
            dbpath.unwrap(),
            Utf8PathBuf::from("var/lib/pacman"),
            "DBPath is correctly parsed"
        );
    }

    #[test]
    fn pacman_conf_dont_detect_invalid_dbpaths() {
        /// A pacman.conf file that contains various invalid forms for `DBPath`, none of which should parse
        pub const PACMAN_CONF_DBPATH_INVALID: &str = r#"
        [options]
        # Comment at the start of the file with leading whitespace

        # Commented out DBPath with leading whitespace:
        #DBPath = /invalid

        # Commented out DBPath without leading whitespace:
#DBPath = /invalid

        # A line starting with "DBPath" without being a valid DBPath:
DBPathInvalid = /invalid

        # Don't parse empty paths
        DBPath = # This one is empty and has an inline comment
        "#;

        /// A pacman.conf file that is missing the [options] section but is otherwise valid (should parse, but not yield a result)
        pub const PACMAN_CONF_MISSING_OPTIONS: &str = "DBPath = /var/lib/pacman";
        let dbpath = AlpmComponentsRepo::parse_pacman_conf_dbpath(PACMAN_CONF_DBPATH_INVALID);
        assert!(dbpath.is_none(), "invalid dbpaths must not be parsed");

        let dbpath = AlpmComponentsRepo::parse_pacman_conf_dbpath(PACMAN_CONF_MISSING_OPTIONS);
        assert!(
            dbpath.is_none(),
            "dbpaths not inside an [options] section must not be parsed"
        );
    }

    #[test]
    fn pacman_conf_last_with_inline_comment() {
        /// A pacman.conf file that is valid, but contains comments in the same line
        pub const PACMAN_CONF_MULTIPLE_WITH_INLINE_COMMENT_AND_RELATIVE: &str = r#"
        [options]
        DBPath = /invalid # This is an inline comment
        [invalid]
        invalid_option = false
        [options]
        DBPath = var/lib/pacman # Last path should win. Test relative paths are working as well.
        "#;
        let dbpath = AlpmComponentsRepo::parse_pacman_conf_dbpath(
            PACMAN_CONF_MULTIPLE_WITH_INLINE_COMMENT_AND_RELATIVE,
        );
        assert_eq!(dbpath.unwrap(), Utf8PathBuf::from("var/lib/pacman"));
    }
}
