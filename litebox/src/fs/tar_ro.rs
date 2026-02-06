// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A read-only tar-backed file system.
//!
//! ```txt
//!                  __
//!                 / /
//!                / /
//!               / /
//!     ================
//!     |       / /    |
//!     |______/_/_____|
//!     \              /
//!      |            |
//!      |            |
//!      \            /
//!       |          |
//!       |  O  O  O |
//!        \O O O O /
//!        | O O O O|
//!        |________|
//!
//! Taro Milk Tea, Tapioca Bubbles, 50% Sugar, No Ice.
//! ```

use alloc::borrow::ToOwned as _;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};

use crate::{
    LiteBox,
    fs::{DirEntry, FileType},
    path::Arg as _,
    sync,
};

use super::{
    Mode, NodeInfo, OFlags, SeekWhence, UserInfo,
    errors::{
        ChmodError, ChownError, CloseError, MkdirError, OpenError, PathError, ReadDirError,
        ReadError, RmdirError, SeekError, TruncateError, UnlinkError, WriteError,
    },
};

/// Just a random constant that is distinct from other file systems. In this case, it is
/// `b'Taro'.hex()`.
const DEVICE_ID: usize = 0x5461726f;

/// TODO(jayb): Replace this proper auto-incrementing inode number storage (although that will
/// require migrating to the hashmap based tar entry storage). This is ok for now, until something
/// is actually checking for real inode numbers.
const TEMPORARY_DEFAULT_CONSTANT_INODE_NUMBER: usize = 0xFACE;

/// Block size for file system I/O operations
// TODO(jayb): Determine appropriate block size
const BLOCK_SIZE: usize = 0;

/// Cached information about a tar entry for O(1) lookups.
#[derive(Clone)]
struct TarEntryInfo {
    /// Index into tar entries iterator
    idx: usize,
    /// File size in bytes
    size: usize,
    /// File mode/permissions
    mode: Mode,
    /// Owner info
    owner: UserInfo,
}

/// Index structure for fast path lookups.
struct TarIndex {
    /// Map from normalized path (without leading /) to entry info
    files: HashMap<String, TarEntryInfo>,
    /// Set of known directory paths (without leading /)
    directories: HashSet<String>,
}

impl TarIndex {
    /// Build an index from tar entries. This is O(n) but only done once.
    fn build(tar_data: &TarData) -> Self {
        let mut files = HashMap::new();
        let mut directories = HashSet::new();

        // Always include root directory
        directories.insert(String::new());

        for (idx, entry) in tar_data.entries().enumerate() {
            let filename_result = entry.filename();
            let Ok(filename) = filename_result.as_str() else {
                continue;
            };
            let path = normalize_tar_filename(filename);

            // Skip empty paths
            if path.is_empty() {
                continue;
            }

            // Add all parent directories
            let mut current = String::new();
            for component in path.split('/') {
                if !current.is_empty() {
                    current.push('/');
                }
                current.push_str(component);

                // If this isn't the final component, it's a directory
                if current.len() < path.len() {
                    directories.insert(current.clone());
                }
            }

            // Check if this entry itself is a directory (ends with / or is a dir type)
            let is_dir = path.ends_with('/');
            let normalized_path = path.trim_end_matches('/').to_owned();

            if is_dir {
                directories.insert(normalized_path);
            } else {
                // It's a file - store entry info
                let mode = entry
                    .posix_header()
                    .mode
                    .to_flags()
                    .map(mode_of_modeflags)
                    .unwrap_or(DEFAULT_DIR_MODE);
                let owner = owner_from_posix_header(entry.posix_header());

                files.insert(
                    normalized_path,
                    TarEntryInfo {
                        idx,
                        size: entry.size(),
                        mode,
                        owner,
                    },
                );
            }
        }

        Self { files, directories }
    }

    /// Look up a file by path. Returns None if not found or if it's a directory.
    fn get_file(&self, path: &str) -> Option<&TarEntryInfo> {
        self.files.get(path)
    }

    /// Check if a path is a directory.
    fn is_directory(&self, path: &str) -> bool {
        self.directories.contains(path)
    }

    /// Check if a path exists (as file or directory).
    fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path) || self.directories.contains(path)
    }

    /// Get all entries in a directory.
    fn list_directory(&self, dir_path: &str) -> Vec<(String, FileType, usize)> {
        let prefix = if dir_path.is_empty() {
            String::new()
        } else {
            format!("{dir_path}/")
        };

        let mut entries: HashMap<String, (FileType, usize)> = HashMap::new();

        // Find files in this directory
        for (path, info) in &self.files {
            if let Some(suffix) = path.strip_prefix(&prefix) {
                // Check if it's a direct child (no more slashes)
                if let Some((name, _)) = suffix.split_once('/') {
                    // It's in a subdirectory
                    entries
                        .entry(name.to_owned())
                        .or_insert((FileType::Directory, TEMPORARY_DEFAULT_CONSTANT_INODE_NUMBER));
                } else if !suffix.is_empty() {
                    // Direct child file
                    entries.insert(suffix.to_owned(), (FileType::RegularFile, info.idx + 1));
                }
            } else if dir_path.is_empty() && !path.contains('/') {
                // Root directory, direct child
                entries.insert(path.clone(), (FileType::RegularFile, info.idx + 1));
            }
        }

        // Find subdirectories
        for dir in &self.directories {
            if let Some(suffix) = dir.strip_prefix(&prefix) {
                if let Some((name, _)) = suffix.split_once('/') {
                    entries
                        .entry(name.to_owned())
                        .or_insert((FileType::Directory, TEMPORARY_DEFAULT_CONSTANT_INODE_NUMBER));
                } else if !suffix.is_empty() {
                    entries
                        .entry(suffix.to_owned())
                        .or_insert((FileType::Directory, TEMPORARY_DEFAULT_CONSTANT_INODE_NUMBER));
                }
            } else if dir_path.is_empty() && !dir.is_empty() && !dir.contains('/') {
                entries
                    .entry(dir.clone())
                    .or_insert((FileType::Directory, TEMPORARY_DEFAULT_CONSTANT_INODE_NUMBER));
            }
        }

        entries
            .into_iter()
            .map(|(name, (ft, ino))| (name, ft, ino))
            .collect()
    }
}

enum TarData {
    Owned(tar_no_std::TarArchive),
    Borrowed(tar_no_std::TarArchiveRef<'static>),
}

impl TarData {
    fn entries(&self) -> tar_no_std::ArchiveEntryIterator<'_> {
        match self {
            TarData::Owned(ar) => ar.entries(),
            TarData::Borrowed(ar_ref) => ar_ref.entries(),
        }
    }
}

/// A backing implementation for [`FileSystem`](super::FileSystem), storing all files in-memory, via
/// a read-only `.tar` file.
pub struct FileSystem<Platform: sync::RawSyncPrimitivesProvider> {
    litebox: LiteBox<Platform>,
    tar_data: TarData,
    /// Index for O(1) path lookups (built once on construction)
    index: TarIndex,
    // cwd invariant: always ends with a `/`
    current_working_dir: String,
}

/// An empty tar file to support an empty file system.
pub const EMPTY_TAR_FILE: &[u8] = &[0u8; 10240];

impl<Platform: sync::RawSyncPrimitivesProvider> FileSystem<Platform> {
    /// Construct a new `FileSystem` instance from provided `tar_data`.
    ///
    /// Note: this function accepts `tar_data` as a `Cow<'static, [u8]>`. When a borrowed slice is
    /// provided the filesystem will use a `TarArchiveRef` without taking ownership; when an owned
    /// buffer is provided it will be consumed to construct a `TarArchive`. Using `Cow` avoids an
    /// unnecessary copy while allowing either borrowed or owned input.
    ///
    /// Use [`EMPTY_TAR_FILE`] if you need an empty file system.
    ///
    /// # Panics
    ///
    /// Panics if the provided `tar_data` is found to be an invalid `.tar` file.
    #[must_use]
    pub fn new(litebox: &LiteBox<Platform>, tar_data: alloc::borrow::Cow<'static, [u8]>) -> Self {
        let tar_data = match tar_data {
            alloc::borrow::Cow::Borrowed(slice) => {
                TarData::Borrowed(tar_no_std::TarArchiveRef::new(slice).expect("invalid tar data"))
            }
            alloc::borrow::Cow::Owned(vec) => TarData::Owned(
                tar_no_std::TarArchive::new(vec.into_boxed_slice()).expect("invalid tar data"),
            ),
        };
        // Build index for O(1) lookups (one-time O(n) cost)
        let index = TarIndex::build(&tar_data);
        Self {
            litebox: litebox.clone(),
            tar_data,
            index,
            current_working_dir: "/".into(),
        }
    }

    /// Gives the absolute path for `path`, resolving any `.` or `..`s, and making sure to account
    /// for any relative paths from current working directory.
    ///
    /// Note: does NOT account for symlinks.
    fn absolute_path(&self, path: impl crate::path::Arg) -> Result<String, PathError> {
        assert!(self.current_working_dir.ends_with('/'));
        let path = path.as_rust_str()?;
        if path.starts_with('/') {
            // Absolute path
            Ok(path.normalized()?)
        } else {
            // Relative path
            Ok((self.current_working_dir.clone() + path.as_rust_str()?).normalized()?)
        }
    }
}

impl<Platform: sync::RawSyncPrimitivesProvider> super::private::Sealed for FileSystem<Platform> {}

/// Strip the `./` prefix from tar filenames if present.
///
/// This is helpful for tar files that have been created via `tar cvf foo.tar .`
fn normalize_tar_filename(filename: &str) -> &str {
    filename.strip_prefix("./").unwrap_or(filename)
}

impl<Platform: sync::RawSyncPrimitivesProvider> super::FileSystem for FileSystem<Platform> {
    fn open(
        &self,
        path: impl crate::path::Arg,
        flags: OFlags,
        _mode: Mode,
    ) -> Result<FileFd<Platform>, OpenError> {
        use super::OFlags;
        let currently_supported_oflags: OFlags = OFlags::RDONLY
            | OFlags::WRONLY
            | OFlags::RDWR
            | OFlags::CREAT
            | OFlags::EXCL
            | OFlags::TRUNC
            | OFlags::NOCTTY
            | OFlags::DIRECTORY
            | OFlags::NONBLOCK
            | OFlags::LARGEFILE
            | OFlags::NOFOLLOW
            | OFlags::APPEND;
        if flags.intersects(currently_supported_oflags.complement()) {
            unimplemented!("{flags:?}")
        }
        if flags.contains(OFlags::CREAT) {
            return Err(OpenError::ReadOnlyFileSystem);
        }
        let path = self.absolute_path(path)?;
        if path.is_empty() {
            // We are at the root directory, we should just return early.
            return Ok(self
                .litebox
                .descriptor_table_mut()
                .insert(Descriptor::Dir { path: path.clone() }));
        }
        assert!(path.starts_with('/'));
        let path = &path[1..];

        // Use index for O(1) lookup instead of linear scan
        if flags.contains(OFlags::RDWR) || flags.contains(OFlags::WRONLY) {
            return Err(OpenError::ReadOnlyFileSystem);
        }
        assert!(flags.contains(OFlags::RDONLY));

        // Check if it's a file
        if let Some(entry_info) = self.index.get_file(path) {
            if flags.contains(OFlags::DIRECTORY) {
                return Err(OpenError::PathError(PathError::ComponentNotADirectory));
            }
            let fd = self
                .litebox
                .descriptor_table_mut()
                .insert(Descriptor::File {
                    idx: entry_info.idx,
                    position: 0,
                });
            if flags.contains(OFlags::TRUNC) {
                match self.truncate(&fd, 0, true) {
                    Ok(()) => {}
                    Err(e) => {
                        self.close(&fd).unwrap();
                        return Err(e.into());
                    }
                }
            }
            return Ok(fd);
        }

        // Check if it's a directory
        if self.index.is_directory(path) {
            return Ok(self.litebox.descriptor_table_mut().insert(Descriptor::Dir {
                path: path.to_owned(),
            }));
        }

        // Not found
        Err(PathError::NoSuchFileOrDirectory)?
    }

    fn close(&self, fd: &FileFd<Platform>) -> Result<(), CloseError> {
        self.litebox.descriptor_table_mut().remove(fd);
        Ok(())
    }

    fn read(
        &self,
        fd: &FileFd<Platform>,
        buf: &mut [u8],
        mut offset: Option<usize>,
    ) -> Result<usize, ReadError> {
        let descriptor_table = self.litebox.descriptor_table();
        let Descriptor::File { idx, position } = &mut descriptor_table
            .get_entry_mut(fd)
            .ok_or(ReadError::ClosedFd)?
            .entry
        else {
            return Err(ReadError::NotAFile);
        };
        let position = offset.as_mut().unwrap_or(position);
        let file = self.tar_data.entries().nth(*idx).unwrap().data();
        let start = (*position).min(file.len());
        let end = position.checked_add(buf.len()).unwrap().min(file.len());
        debug_assert!(start <= end);
        let retlen = end - start;
        buf[..retlen].copy_from_slice(&file[start..end]);
        *position = end;
        Ok(retlen)
    }

    fn write(
        &self,
        fd: &FileFd<Platform>,
        _buf: &[u8],
        _offset: Option<usize>,
    ) -> Result<usize, WriteError> {
        match self
            .litebox
            .descriptor_table()
            .get_entry(fd)
            .ok_or(WriteError::ClosedFd)?
            .entry
        {
            Descriptor::File { .. } => Err(WriteError::NotForWriting),
            Descriptor::Dir { .. } => Err(WriteError::NotAFile),
        }
    }

    fn seek(
        &self,
        fd: &FileFd<Platform>,
        offset: isize,
        whence: SeekWhence,
    ) -> Result<usize, SeekError> {
        let descriptor_table = self.litebox.descriptor_table();
        let Descriptor::File { idx, position } = &mut descriptor_table
            .get_entry_mut(fd)
            .ok_or(SeekError::ClosedFd)?
            .entry
        else {
            return Err(SeekError::NotAFile);
        };
        let file_len = self.tar_data.entries().nth(*idx).unwrap().data().len();
        let base = match whence {
            SeekWhence::RelativeToBeginning => 0,
            SeekWhence::RelativeToCurrentOffset => *position,
            SeekWhence::RelativeToEnd => file_len,
        };
        let new_posn = base
            .checked_add_signed(offset)
            .ok_or(SeekError::InvalidOffset)?;
        if new_posn > file_len {
            Err(SeekError::InvalidOffset)
        } else {
            *position = new_posn;
            Ok(new_posn)
        }
    }

    fn truncate(
        &self,
        fd: &FileFd<Platform>,
        _length: usize,
        _reset_offset: bool,
    ) -> Result<(), TruncateError> {
        match self
            .litebox
            .descriptor_table()
            .get_entry(fd)
            .ok_or(TruncateError::ClosedFd)?
            .entry
        {
            Descriptor::File { .. } => Err(TruncateError::NotForWriting),
            Descriptor::Dir { .. } => Err(TruncateError::IsDirectory),
        }
    }

    fn chmod(&self, path: impl crate::path::Arg, _mode: Mode) -> Result<(), ChmodError> {
        let path = self.absolute_path(path)?;
        let path = if path.is_empty() {
            ""
        } else {
            assert!(path.starts_with('/'));
            &path[1..]
        };
        // Use index for O(1) lookup
        if self.index.exists(path) {
            Err(ChmodError::ReadOnlyFileSystem)
        } else {
            Err(PathError::NoSuchFileOrDirectory)?
        }
    }

    fn chown(
        &self,
        path: impl crate::path::Arg,
        _user: Option<u16>,
        _group: Option<u16>,
    ) -> Result<(), ChownError> {
        let path = self.absolute_path(path)?;
        let path = if path.is_empty() {
            ""
        } else {
            assert!(path.starts_with('/'));
            &path[1..]
        };
        // Use index for O(1) lookup
        if self.index.exists(path) {
            Err(ChownError::ReadOnlyFileSystem)
        } else {
            Err(PathError::NoSuchFileOrDirectory)?
        }
    }

    fn unlink(&self, path: impl crate::path::Arg) -> Result<(), UnlinkError> {
        let path = self.absolute_path(path)?;
        assert!(path.starts_with('/'));
        let path = &path[1..];
        // Use index for O(1) lookup
        if self.index.get_file(path).is_some() {
            Err(UnlinkError::ReadOnlyFileSystem)
        } else if self.index.is_directory(path) {
            Err(UnlinkError::IsADirectory)
        } else {
            Err(PathError::NoSuchFileOrDirectory)?
        }
    }

    fn mkdir(&self, _path: impl crate::path::Arg, _mode: Mode) -> Result<(), MkdirError> {
        // TODO: Do we need to do the type of checks that are happening in the other functions, or
        // should the other functions be simplified to this?
        Err(MkdirError::ReadOnlyFileSystem)
    }

    fn rmdir(&self, _path: impl crate::path::Arg) -> Result<(), RmdirError> {
        // TODO: Do we need to do the type of checks that are happening in the other functions, or
        // should the other functions be simplified to this?
        Err(RmdirError::ReadOnlyFileSystem)
    }

    fn read_dir(&self, fd: &FileFd<Platform>) -> Result<Vec<DirEntry>, ReadDirError> {
        let descriptor_table = self.litebox.descriptor_table();
        let Descriptor::Dir { path } = &descriptor_table
            .get_entry(fd)
            .ok_or(ReadDirError::ClosedFd)?
            .entry
        else {
            return Err(ReadDirError::NotADirectory);
        };

        // Use index for O(1) directory listing
        let entries = self.index.list_directory(path);

        // Add "." and ".." entries first.
        let mut out: Vec<DirEntry> = Vec::new();

        out.push(DirEntry {
            name: ".".into(),
            file_type: FileType::Directory,
            ino_info: Some(NodeInfo {
                dev: DEVICE_ID,
                ino: TEMPORARY_DEFAULT_CONSTANT_INODE_NUMBER,
                rdev: None,
            }),
        });

        out.push(DirEntry {
            name: "..".into(),
            file_type: FileType::Directory,
            ino_info: Some(NodeInfo {
                dev: DEVICE_ID,
                ino: TEMPORARY_DEFAULT_CONSTANT_INODE_NUMBER,
                rdev: None,
            }),
        });

        out.extend(entries.into_iter().map(|(name, file_type, ino)| DirEntry {
            name,
            file_type,
            ino_info: Some(NodeInfo {
                dev: DEVICE_ID,
                ino,
                rdev: None,
            }),
        }));
        Ok(out)
    }

    fn file_status(
        &self,
        path: impl crate::path::Arg,
    ) -> Result<super::FileStatus, super::errors::FileStatusError> {
        let path = self.absolute_path(path)?;
        let path = if path.is_empty() {
            ""
        } else {
            assert!(path.starts_with('/'));
            &path[1..]
        };

        // Use index for O(1) lookup
        if let Some(entry_info) = self.index.get_file(path) {
            return Ok(super::FileStatus {
                file_type: super::FileType::RegularFile,
                mode: entry_info.mode,
                size: entry_info.size,
                owner: entry_info.owner,
                node_info: NodeInfo {
                    dev: DEVICE_ID,
                    // ino starts at 1 (zero represents deleted file)
                    ino: entry_info.idx + 1,
                    rdev: None,
                },
                blksize: BLOCK_SIZE,
            });
        }

        if self.index.is_directory(path) {
            return Ok(super::FileStatus {
                file_type: super::FileType::Directory,
                mode: DEFAULT_DIR_MODE,
                size: super::DEFAULT_DIRECTORY_SIZE,
                owner: DEFAULT_DIRECTORY_OWNER,
                node_info: NodeInfo {
                    dev: DEVICE_ID,
                    ino: TEMPORARY_DEFAULT_CONSTANT_INODE_NUMBER,
                    rdev: None,
                },
                blksize: BLOCK_SIZE,
            });
        }

        Err(PathError::NoSuchFileOrDirectory)?
    }

    fn fd_file_status(
        &self,
        fd: &FileFd<Platform>,
    ) -> Result<super::FileStatus, super::errors::FileStatusError> {
        match &self
            .litebox
            .descriptor_table()
            .get_entry(fd)
            .ok_or(super::errors::FileStatusError::ClosedFd)?
            .entry
        {
            Descriptor::File { idx, .. } => {
                let entry = self.tar_data.entries().nth(*idx).unwrap();
                Ok(super::FileStatus {
                    file_type: super::FileType::RegularFile,
                    mode: mode_of_modeflags(entry.posix_header().mode.to_flags().unwrap()),
                    size: entry.size(),
                    owner: owner_from_posix_header(entry.posix_header()),
                    node_info: NodeInfo {
                        dev: DEVICE_ID,
                        // ino starts at 1 (zero represents deleted file)
                        ino: *idx + 1,
                        rdev: None,
                    },
                    blksize: BLOCK_SIZE,
                })
            }
            Descriptor::Dir { .. } => Ok(super::FileStatus {
                file_type: super::FileType::Directory,
                mode: DEFAULT_DIR_MODE,
                size: super::DEFAULT_DIRECTORY_SIZE,
                owner: DEFAULT_DIRECTORY_OWNER,
                node_info: NodeInfo {
                    dev: DEVICE_ID,
                    ino: TEMPORARY_DEFAULT_CONSTANT_INODE_NUMBER,
                    rdev: None,
                },
                blksize: BLOCK_SIZE,
            }),
        }
    }
}

const DEFAULT_DIR_MODE: Mode =
    Mode::from_bits(Mode::RWXU.bits() | Mode::RWXG.bits() | Mode::RWXO.bits()).unwrap();

const DEFAULT_DIRECTORY_OWNER: UserInfo = UserInfo {
    user: 1000,
    group: 1000,
};

fn mode_of_modeflags(perms: tar_no_std::ModeFlags) -> Mode {
    use tar_no_std::ModeFlags;
    let mut mode = Mode::empty();
    mode.set(Mode::RUSR, perms.contains(ModeFlags::OwnerRead));
    mode.set(Mode::WUSR, perms.contains(ModeFlags::OwnerWrite));
    mode.set(Mode::XUSR, perms.contains(ModeFlags::OwnerExec));
    mode.set(Mode::RGRP, perms.contains(ModeFlags::GroupRead));
    mode.set(Mode::WGRP, perms.contains(ModeFlags::GroupWrite));
    mode.set(Mode::XGRP, perms.contains(ModeFlags::GroupExec));
    mode.set(Mode::ROTH, perms.contains(ModeFlags::OthersRead));
    mode.set(Mode::WOTH, perms.contains(ModeFlags::OthersWrite));
    mode.set(Mode::XOTH, perms.contains(ModeFlags::OthersExec));
    mode
}

fn owner_from_posix_header(posix_header: &tar_no_std::PosixHeader) -> UserInfo {
    UserInfo {
        user: posix_header.uid.as_number().unwrap(),
        group: posix_header.gid.as_number().unwrap(),
    }
}

enum Descriptor {
    File { idx: usize, position: usize },
    Dir { path: String },
}

crate::fd::enable_fds_for_subsystem! {
    @ Platform: { sync::RawSyncPrimitivesProvider };
    FileSystem<Platform>;
    Descriptor;
    -> FileFd<Platform>;
}
