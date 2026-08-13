use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
#[cfg(unix)]
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_fs_ext::{OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_primitives::fs::FollowSymlinks;
use cap_std::fs::{Dir, File, Metadata};

use super::{ConversationSourceError, ConversationSourceErrorKind, SourceId, StorageProbe};
use crate::project::ProjectIdentity;

const MAX_REGISTERED_LOCATIONS: usize = 16;
const MAX_LOCATION_PATH_BYTES: usize = 1_024;
const MAX_SHALLOW_ENTRIES: usize = 128;
const MAX_DISCOVERABLE_FILES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectLocalLocation(PathBuf);

impl ProjectLocalLocation {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, ProjectLocalLocationError> {
        let path = path.into();
        if path.as_os_str().is_empty() {
            return Err(ProjectLocalLocationError::new(
                path,
                "location must not be empty",
            ));
        }
        if path.as_os_str().as_encoded_bytes().len() > MAX_LOCATION_PATH_BYTES {
            return Err(ProjectLocalLocationError::new(
                path,
                "location exceeds the path length limit",
            ));
        }
        if !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(ProjectLocalLocationError::new(
                path,
                "location must be a normalized project-relative path",
            ));
        }
        Ok(Self(path))
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectLocalLocationError {
    path: PathBuf,
    reason: &'static str,
}

impl ProjectLocalLocationError {
    const fn new(path: PathBuf, reason: &'static str) -> Self {
        Self { path, reason }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Display for ProjectLocalLocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid project-local location {}: {}",
            self.path.display(),
            self.reason
        )
    }
}

impl Error for ProjectLocalLocationError {}

#[derive(Debug)]
pub(super) struct ProjectLocalFiles {
    project: ProjectIdentity,
    root: Arc<Dir>,
    locations: Box<[ProjectLocalLocation]>,
}

#[derive(Debug)]
pub(super) struct ProjectLocalListing {
    pub(super) files: Vec<ProjectLocalFile>,
    pub(super) errors: Vec<ConversationSourceError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ProjectLocalFile {
    relative_path: PathBuf,
    absolute_path: PathBuf,
}

impl ProjectLocalFile {
    pub(super) fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub(super) fn absolute_path(&self) -> &Path {
        &self.absolute_path
    }
}

impl ProjectLocalFiles {
    pub(super) fn new(
        source_id: &SourceId,
        project: ProjectIdentity,
        locations: impl IntoIterator<Item = ProjectLocalLocation>,
    ) -> Result<Self, ConversationSourceError> {
        let locations = locations.into_iter().collect::<Vec<_>>();
        if locations.is_empty() || locations.len() > MAX_REGISTERED_LOCATIONS {
            return Err(ConversationSourceError::new(
                source_id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "project-local source requires between one and sixteen registered locations",
            ));
        }
        let root = open_ambient_directory_nofollow(project.root()).map_err(|error| {
            io_error(
                source_id,
                project.root().to_path_buf(),
                "project root is unavailable",
                &error,
            )
        })?;
        Ok(Self {
            project,
            root: Arc::new(root),
            locations: locations.into_boxed_slice(),
        })
    }

    pub(super) fn probe(
        &self,
        source_id: &SourceId,
    ) -> Result<StorageProbe, ConversationSourceError> {
        let mut first_error = None;
        for location in &self.locations {
            match self.symlink_metadata(location.as_path()) {
                Ok(metadata) if !metadata.is_symlink() => return Ok(StorageProbe::Available),
                Ok(_) => {
                    first_error.get_or_insert_with(|| {
                        ConversationSourceError::new(
                            source_id.clone(),
                            ConversationSourceErrorKind::InvalidData,
                            "registered project-local location is a symlink",
                        )
                        .with_path(self.absolute(location.as_path()))
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    first_error.get_or_insert_with(|| {
                        io_error(
                            source_id,
                            self.absolute(location.as_path()),
                            "registered project-local location is unreadable",
                            &error,
                        )
                    });
                }
            }
        }
        first_error.map_or_else(
            || {
                Ok(StorageProbe::Unavailable {
                    reason: "no registered project-local locations exist".to_owned(),
                })
            },
            Err,
        )
    }

    pub(super) fn list_files(
        &self,
        source_id: &SourceId,
        accepted_extension: impl Fn(&Path) -> bool,
    ) -> ProjectLocalListing {
        let mut files = Vec::new();
        let mut errors = Vec::new();
        for location in &self.locations {
            self.list_location(
                source_id,
                location.as_path(),
                &accepted_extension,
                &mut files,
                &mut errors,
            );
        }
        files.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
        files.dedup_by(|left, right| left.relative_path == right.relative_path);
        if files.len() > MAX_DISCOVERABLE_FILES {
            files.clear();
            errors.push(
                ConversationSourceError::new(
                    source_id.clone(),
                    ConversationSourceErrorKind::InvalidData,
                    "registered project-local locations exceed the total file limit",
                )
                .with_path(self.project.root().to_path_buf()),
            );
        }
        ProjectLocalListing { files, errors }
    }

    fn list_location(
        &self,
        source_id: &SourceId,
        location: &Path,
        accepted_extension: &impl Fn(&Path) -> bool,
        files: &mut Vec<ProjectLocalFile>,
        errors: &mut Vec<ConversationSourceError>,
    ) {
        let metadata = match self.symlink_metadata(location) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                errors.push(io_error(
                    source_id,
                    self.absolute(location),
                    "registered project-local location is unreadable",
                    &error,
                ));
                return;
            }
        };
        if metadata.is_symlink() {
            errors.push(
                ConversationSourceError::new(
                    source_id.clone(),
                    ConversationSourceErrorKind::InvalidData,
                    "registered project-local location is a symlink",
                )
                .with_path(self.absolute(location)),
            );
            return;
        }
        if metadata.is_file() {
            if accepted_extension(location) {
                files.push(self.file(location.to_path_buf()));
            }
            return;
        }
        if !metadata.is_dir() {
            return;
        }

        let directory = match self.open_directory(location) {
            Ok(directory) => directory,
            Err(error) => {
                errors.push(io_error(
                    source_id,
                    self.absolute(location),
                    "registered project-local directory is unreadable",
                    &error,
                ));
                return;
            }
        };
        let entries = match directory.entries() {
            Ok(entries) => entries,
            Err(error) => {
                errors.push(io_error(
                    source_id,
                    self.absolute(location),
                    "registered project-local directory cannot be listed",
                    &error,
                ));
                return;
            }
        };
        let mut names = Vec::new();
        for result in entries.take(MAX_SHALLOW_ENTRIES + 1) {
            match result {
                Ok(entry) => names.push(entry.file_name()),
                Err(error) => errors.push(io_error(
                    source_id,
                    self.absolute(location),
                    "project-local directory entry is unreadable",
                    &error,
                )),
            }
        }
        if names.len() > MAX_SHALLOW_ENTRIES {
            errors.push(
                ConversationSourceError::new(
                    source_id.clone(),
                    ConversationSourceErrorKind::InvalidData,
                    "registered project-local directory exceeds the shallow entry limit",
                )
                .with_path(self.absolute(location)),
            );
            return;
        }
        names.sort_unstable();
        for name in names {
            let relative = location.join(&name);
            let metadata = match directory.symlink_metadata(&name) {
                Ok(metadata) => metadata,
                Err(error) => {
                    errors.push(io_error(
                        source_id,
                        self.absolute(&relative),
                        "project-local entry is unreadable",
                        &error,
                    ));
                    continue;
                }
            };
            if metadata.is_symlink() {
                errors.push(
                    ConversationSourceError::new(
                        source_id.clone(),
                        ConversationSourceErrorKind::InvalidData,
                        "project-local conversation file is a symlink",
                    )
                    .with_path(self.absolute(&relative)),
                );
            } else if metadata.is_file() && accepted_extension(&relative) {
                files.push(self.file(relative));
            }
        }
    }

    pub(super) fn open_registered_file(
        &self,
        source_id: &SourceId,
        absolute_path: &Path,
    ) -> Result<(File, Metadata), ConversationSourceError> {
        let relative = absolute_path
            .strip_prefix(self.project.root())
            .map_err(|_| {
                ConversationSourceError::new(
                    source_id.clone(),
                    ConversationSourceErrorKind::ProjectMismatch,
                    "conversation candidate is outside the canonical project root",
                )
                .with_path(absolute_path.to_path_buf())
            })?;
        if !is_normal_relative_path(relative) || !self.is_registered_file(relative) {
            return Err(ConversationSourceError::new(
                source_id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "conversation candidate is not in a registered shallow location",
            )
            .with_path(absolute_path.to_path_buf()));
        }
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let name = relative.file_name().ok_or_else(|| {
            ConversationSourceError::new(
                source_id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "conversation candidate has no file name",
            )
            .with_path(absolute_path.to_path_buf())
        })?;
        let directory = self.open_directory(parent).map_err(|error| {
            io_error(
                source_id,
                absolute_path.to_path_buf(),
                "conversation candidate directory is unavailable",
                &error,
            )
        })?;
        let file = open_file_nofollow(&directory, name).map_err(|error| {
            io_error(
                source_id,
                absolute_path.to_path_buf(),
                "conversation candidate is unreadable",
                &error,
            )
        })?;
        let metadata = file.metadata().map_err(|error| {
            io_error(
                source_id,
                absolute_path.to_path_buf(),
                "conversation candidate metadata is unreadable",
                &error,
            )
        })?;
        if !metadata.is_file() {
            return Err(ConversationSourceError::new(
                source_id.clone(),
                ConversationSourceErrorKind::InvalidData,
                "conversation candidate is not a regular file",
            )
            .with_path(absolute_path.to_path_buf()));
        }
        Ok((file, metadata))
    }

    pub(super) fn is_registered_file(&self, relative: &Path) -> bool {
        self.locations.iter().any(|location| {
            relative == location.as_path() || relative.parent() == Some(location.as_path())
        })
    }

    fn file(&self, relative_path: PathBuf) -> ProjectLocalFile {
        ProjectLocalFile {
            absolute_path: self.absolute(&relative_path),
            relative_path,
        }
    }

    fn absolute(&self, relative: &Path) -> PathBuf {
        self.project.root().join(relative)
    }

    fn symlink_metadata(&self, relative: &Path) -> io::Result<Metadata> {
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let name = relative
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?;
        self.open_directory(parent)?.symlink_metadata(name)
    }

    fn open_directory(&self, relative: &Path) -> io::Result<Dir> {
        if !relative.as_os_str().is_empty() && !is_normal_relative_path(relative) {
            return Err(invalid_relative_path());
        }
        let mut directory = self.root.try_clone()?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(invalid_relative_path());
            };
            directory = open_child_directory_nofollow(&directory, name)?;
        }
        Ok(directory)
    }
}

fn is_normal_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn io_error(
    source_id: &SourceId,
    path: PathBuf,
    message: &'static str,
    error: &io::Error,
) -> ConversationSourceError {
    let kind = if error.kind() == io::ErrorKind::PermissionDenied {
        ConversationSourceErrorKind::PermissionDenied
    } else if error.kind() == io::ErrorKind::InvalidInput {
        ConversationSourceErrorKind::InvalidData
    } else {
        ConversationSourceErrorKind::Io
    };
    ConversationSourceError::new(source_id.clone(), kind, message).with_path(path)
}

fn invalid_relative_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "path must contain only normalized relative components",
    )
}

#[cfg(unix)]
fn open_ambient_directory_nofollow(path: &Path) -> io::Result<Dir> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open("/")?;
    let mut directory = Dir::from_std_file(file);
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_child_directory_nofollow(&directory, name)?;
            }
            _ => return Err(invalid_relative_path()),
        }
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn open_ambient_directory_nofollow(path: &Path) -> io::Result<Dir> {
    let mut ambient_root = PathBuf::new();
    let mut has_root = false;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) if !has_root => ambient_root.push(prefix.as_os_str()),
            Component::RootDir if !has_root => {
                ambient_root.push(component.as_os_str());
                has_root = true;
            }
            Component::Normal(_) => break,
            _ => return Err(invalid_relative_path()),
        }
    }
    if !has_root {
        return Err(invalid_relative_path());
    }
    let mut directory = Dir::open_ambient_dir(ambient_root, cap_std::ambient_authority())?;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_child_directory_nofollow(&directory, name)?;
            }
            _ => return Err(invalid_relative_path()),
        }
    }
    Ok(directory)
}

fn open_child_directory_nofollow(parent: &Dir, name: &OsStr) -> io::Result<Dir> {
    use cap_std::fs::OpenOptions;

    let mut options = OpenOptions::new();
    options.read(true);
    OpenOptionsFollowExt::follow(&mut options, FollowSymlinks::No);
    OpenOptionsMaybeDirExt::maybe_dir(&mut options, true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    let file = parent.open_with(name, &options)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path component is not a directory",
        ));
    }
    Ok(Dir::from_std_file(file.into_std()))
}

fn open_file_nofollow(parent: &Dir, name: &OsStr) -> io::Result<File> {
    use cap_std::fs::OpenOptions;

    let mut options = OpenOptions::new();
    options.read(true);
    OpenOptionsFollowExt::follow(&mut options, FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    parent.open_with(name, &options)
}
