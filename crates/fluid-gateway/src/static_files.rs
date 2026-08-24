#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::fs::{File, Metadata};
use std::io::{self, Read};
#[cfg(not(unix))]
use std::path::PathBuf;
use std::path::{Component, Path};
use std::sync::Arc;
use std::time::SystemTime;

const MAX_STATIC_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_EAGAIN_RETRIES: usize = 8;

#[derive(Clone)]
pub(crate) struct StaticRoot {
    file: Arc<File>,
    #[cfg(not(unix))]
    path: PathBuf,
    #[cfg(not(unix))]
    generation: Generation,
}

pub(crate) struct ContainedFile {
    pub(crate) bytes: Vec<u8>,
    pub(crate) modified: Option<SystemTime>,
    generation: Generation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadError {
    Missing,
    Forbidden,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Generation {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
    #[cfg(windows)]
    volume_serial_number: Option<u32>,
    #[cfg(windows)]
    file_index: Option<u64>,
    #[cfg(windows)]
    creation_time: u64,
    #[cfg(windows)]
    last_write_time: u64,
}

impl StaticRoot {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        if !file.metadata()?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "deployment root is not a directory",
            ));
        }
        #[cfg(not(unix))]
        let root_generation = generation(&file.metadata()?);
        Ok(Self {
            file: Arc::new(file),
            #[cfg(not(unix))]
            path: std::fs::canonicalize(path)?,
            #[cfg(not(unix))]
            generation: root_generation,
        })
    }
}

pub(crate) async fn read(
    root: &StaticRoot,
    static_dir: &Path,
    relative: &Path,
) -> Result<ContainedFile, ReadError> {
    let root = root.clone();
    let static_dir = static_dir.to_owned();
    let relative = relative.to_owned();
    tokio::task::spawn_blocking(move || read_blocking(&root, &static_dir, &relative))
        .await
        .map_err(|_| ReadError::Unavailable)?
}

/// Read a precompressed sibling, then prove the source still names the exact
/// generation whose bytes were selected. Only a missing sidecar is a benign
/// cache miss; a forbidden or unavailable sidecar fails closed.
pub(crate) async fn read_variant(
    root: &StaticRoot,
    static_dir: &Path,
    relative: &Path,
    source_relative: &Path,
    source: &ContainedFile,
) -> Result<ContainedFile, ReadError> {
    let root = root.clone();
    let static_dir = static_dir.to_owned();
    let relative = relative.to_owned();
    let source_relative = source_relative.to_owned();
    let source_generation = source.generation.clone();
    tokio::task::spawn_blocking(move || {
        let variant = read_blocking(&root, &static_dir, &relative)?;
        let current = open_regular(&root, &static_dir, &source_relative)
            .and_then(|file| metadata_retry(&file))
            .map_err(|error| classify(error, true))?;
        if generation(&current) != source_generation {
            return Err(ReadError::Unavailable);
        }
        Ok(variant)
    })
    .await
    .map_err(|_| ReadError::Unavailable)?
}

fn read_blocking(
    root: &StaticRoot,
    static_dir: &Path,
    relative: &Path,
) -> Result<ContainedFile, ReadError> {
    let mut file = open_regular(root, static_dir, relative).map_err(|e| classify(e, false))?;
    let before = metadata_retry(&file).map_err(|e| classify(e, true))?;
    if !before.is_file() {
        return Err(ReadError::Missing);
    }
    if before.len() > MAX_STATIC_FILE_BYTES {
        return Err(ReadError::Unavailable);
    }

    let expected = before.len() as usize;
    let mut bytes = Vec::with_capacity(expected);
    let mut chunk = [0u8; 64 * 1024];
    let mut eagain = 0usize;
    while bytes.len() <= expected {
        let remaining = expected.saturating_add(1).saturating_sub(bytes.len());
        let limit = remaining.min(chunk.len());
        match file.read(&mut chunk[..limit]) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if is_eagain(&error) && eagain < MAX_EAGAIN_RETRIES => {
                eagain += 1;
                std::thread::yield_now();
            }
            Err(error) => return Err(classify(error, true)),
        }
    }
    let after = metadata_retry(&file).map_err(|e| classify(e, true))?;
    if bytes.len() != expected || generation(&before) != generation(&after) {
        return Err(ReadError::Unavailable);
    }
    Ok(ContainedFile {
        bytes,
        modified: before.modified().ok(),
        generation: generation(&before),
    })
}

fn metadata_retry(file: &File) -> io::Result<Metadata> {
    let mut eagain = 0usize;
    loop {
        match file.metadata() {
            Ok(metadata) => return Ok(metadata),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if is_eagain(&error) && eagain < MAX_EAGAIN_RETRIES => {
                eagain += 1;
                std::thread::yield_now();
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_eagain(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK)
}

fn classify(error: io::Error, opened: bool) -> ReadError {
    if error.kind() == io::ErrorKind::NotFound {
        return if opened {
            ReadError::Unavailable
        } else {
            ReadError::Missing
        };
    }
    if error.kind() == io::ErrorKind::PermissionDenied
        || matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EXDEV || code == libc::ELOOP || code == libc::ENOTDIR
        )
    {
        return ReadError::Forbidden;
    }
    // openat2 is the security boundary on Linux. Falling back after ENOSYS would
    // silently discard RESOLVE_BENEATH/NO_MAGICLINKS, so unsupported kernels are
    // unavailable rather than less protected.
    ReadError::Unavailable
}

fn validate_relative(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "static asset path escapes its deployment root",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_regular(root: &StaticRoot, static_dir: &Path, relative: &Path) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    validate_relative(static_dir)?;
    validate_relative(relative)?;

    let open_beneath = |parent: &File, path: &Path, flags: i32| -> io::Result<File> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "static asset path contains a NUL byte",
            )
        })?;
        let how = OpenHow {
            flags: flags as u64,
            mode: 0,
            resolve: RESOLVE_NO_MAGICLINKS | RESOLVE_BENEATH,
        };
        let mut eagain = 0usize;
        loop {
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_openat2,
                    parent.as_raw_fd(),
                    path.as_ptr(),
                    &how,
                    std::mem::size_of::<OpenHow>(),
                )
            };
            if fd >= 0 {
                return Ok(unsafe { File::from_raw_fd(fd as i32) });
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if is_eagain(&error) && eagain < MAX_EAGAIN_RETRIES {
                eagain += 1;
                std::thread::yield_now();
                continue;
            }
            return Err(error);
        }
    };

    let base = open_beneath(
        &root.file,
        static_dir,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
    )?;
    open_beneath(&base, relative, libc::O_RDONLY | libc::O_CLOEXEC)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_regular(root: &StaticRoot, static_dir: &Path, relative: &Path) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    validate_relative(static_dir)?;
    validate_relative(relative)?;
    let mut parent = root.file.try_clone()?;
    let components: Vec<_> = static_dir
        .components()
        .chain(relative.components())
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_owned()),
            Component::CurDir => None,
            _ => None,
        })
        .collect();
    if components.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "empty static path",
        ));
    }
    for (index, component) in components.iter().enumerate() {
        let name = std::ffi::CString::new(component.as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "static asset path contains a NUL byte",
            )
        })?;
        let last = index + 1 == components.len();
        let flags = if last {
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW
        };
        let mut eagain = 0usize;
        loop {
            let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
            if fd >= 0 {
                parent = unsafe { File::from_raw_fd(fd) };
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if is_eagain(&error) && eagain < MAX_EAGAIN_RETRIES {
                eagain += 1;
                std::thread::yield_now();
                continue;
            }
            return Err(error);
        }
    }
    Ok(parent)
}

#[cfg(not(unix))]
fn open_regular(root: &StaticRoot, static_dir: &Path, relative: &Path) -> io::Result<File> {
    validate_relative(static_dir)?;
    validate_relative(relative)?;
    let root_metadata = std::fs::metadata(&root.path).map_err(io::Error::other)?;
    if generation(&root_metadata) != root.generation {
        return Err(io::Error::other("deployment root identity changed"));
    }
    let base = std::fs::canonicalize(root.path.join(static_dir))?;
    if !base.starts_with(&root.path) || !base.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "static directory escapes its deployment root",
        ));
    }
    let candidate = std::fs::canonicalize(base.join(relative))?;
    if !candidate.starts_with(&base) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "static asset escapes its static directory",
        ));
    }
    let file = File::open(&candidate)?;
    let opened = file.metadata()?;
    let current_root = std::fs::canonicalize(&root.path).map_err(io::Error::other)?;
    let current_candidate = std::fs::canonicalize(&candidate).map_err(io::Error::other)?;
    let current_metadata = std::fs::metadata(&current_candidate).map_err(io::Error::other)?;
    if current_root != root.path
        || current_candidate != candidate
        || generation(&std::fs::metadata(&root.path).map_err(io::Error::other)?) != root.generation
        || generation(&opened) != generation(&current_metadata)
    {
        return Err(io::Error::other(
            "static asset identity changed while opening",
        ));
    }
    Ok(file)
}

fn generation(metadata: &Metadata) -> Generation {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    #[cfg(windows)]
    use std::os::windows::fs::MetadataExt;
    Generation {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        dev: metadata.dev(),
        #[cfg(unix)]
        ino: metadata.ino(),
        #[cfg(unix)]
        ctime: metadata.ctime(),
        #[cfg(unix)]
        ctime_nsec: metadata.ctime_nsec(),
        #[cfg(windows)]
        volume_serial_number: metadata.volume_serial_number(),
        #[cfg(windows)]
        file_index: metadata.file_index(),
        #[cfg(windows)]
        creation_time: metadata.creation_time(),
        #[cfg(windows)]
        last_write_time: metadata.last_write_time(),
    }
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

#[cfg(target_os = "linux")]
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
#[cfg(target_os = "linux")]
const RESOLVE_BENEATH: u64 = 0x08;
