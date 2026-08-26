use anyhow::{bail, Context};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

const INCOMING_DIRECTORY: &str = "incoming-v1";
const STATE_FILE: &str = "state-v1.bin";
const STATE_TEMP_FILE: &str = ".state-v1.tmp";
const PACKAGE_FILE: &str = "package-v1.partial";
const MAX_STATE_BYTES: u64 = 2 * 1024 * 1024;

pub struct TransferFs {
    store_path: PathBuf,
    store: File,
    incoming_path: PathBuf,
    incoming: File,
}

pub struct TransactionDirectory {
    file: File,
}

impl TransferFs {
    pub fn open(store_path: &Path) -> anyhow::Result<Self> {
        let store_path = absolute(store_path)?;
        let store = ensure_absolute_directory(&store_path, 0o750)
            .context("open runtime artifact content store descriptor-relatively")?;
        ensure_directory_at(&store, OsStr::new(INCOMING_DIRECTORY), 0o750)?;
        let incoming = open_directory_at(&store, OsStr::new(INCOMING_DIRECTORY))?;
        let incoming_path = store_path.join(INCOMING_DIRECTORY);
        Ok(Self {
            store_path,
            store,
            incoming_path,
            incoming,
        })
    }

    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    pub fn create_transaction(&self, transaction_id: &str) -> anyhow::Result<TransactionDirectory> {
        validate_component(transaction_id)?;
        let name = OsStr::new(transaction_id);
        match mkdir_at(&self.incoming, name, 0o700) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                bail!("runtime artifact transfer transaction already exists")
            }
            Err(error) => {
                return Err(error).context("create runtime artifact transfer transaction")
            }
        }
        self.incoming.sync_all()?;
        let file = open_directory_at(&self.incoming, name)?;
        Ok(TransactionDirectory { file })
    }

    pub fn open_transaction(&self, transaction_id: &str) -> anyhow::Result<TransactionDirectory> {
        validate_component(transaction_id)?;
        let file = open_directory_at(&self.incoming, OsStr::new(transaction_id))?;
        Ok(TransactionDirectory { file })
    }

    pub fn transaction_ids(&self) -> anyhow::Result<Vec<String>> {
        let names = directory_names(&self.incoming)?;
        let mut ids = Vec::new();
        for name in names {
            let Some(name) = name.to_str() else {
                continue;
            };
            if validate_component(name).is_ok()
                && open_directory_at(&self.incoming, OsStr::new(name)).is_ok()
            {
                ids.push(name.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    pub fn available_bytes(&self) -> anyhow::Result<u64> {
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        let rc = unsafe { libc::fstatvfs(self.incoming.as_raw_fd(), stats.as_mut_ptr()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("measure transfer filesystem");
        }
        let stats = unsafe { stats.assume_init() };
        (stats.f_bavail as u64)
            .checked_mul(stats.f_frsize as u64)
            .context("transfer filesystem capacity overflow")
    }

    pub fn sync_store(&self) -> anyhow::Result<()> {
        self.store.sync_all()?;
        self.incoming.sync_all()?;
        Ok(())
    }

    pub fn incoming_path(&self) -> &Path {
        &self.incoming_path
    }
}

impl TransactionDirectory {
    pub fn write_state(&self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.is_empty() || bytes.len() as u64 > MAX_STATE_BYTES {
            bail!("runtime artifact transfer state exceeds its fixed limit");
        }
        remove_at_if_present(&self.file, OsStr::new(STATE_TEMP_FILE))?;
        let mut temporary = create_regular_at(&self.file, OsStr::new(STATE_TEMP_FILE), 0o600)?;
        temporary.write_all(bytes)?;
        temporary.sync_all()?;
        rename_at(
            &self.file,
            OsStr::new(STATE_TEMP_FILE),
            &self.file,
            OsStr::new(STATE_FILE),
        )?;
        self.file.sync_all()?;
        Ok(())
    }

    pub fn read_state(&self) -> anyhow::Result<Vec<u8>> {
        let mut file = open_regular_at(&self.file, OsStr::new(STATE_FILE), libc::O_RDONLY)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_STATE_BYTES {
            bail!("runtime artifact transfer state file has invalid type or size");
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes)?;
        if bytes.len() as u64 != metadata.len() {
            bail!("runtime artifact transfer state file changed while reading");
        }
        Ok(bytes)
    }

    pub fn create_package(&self) -> anyhow::Result<File> {
        create_regular_at(&self.file, OsStr::new(PACKAGE_FILE), 0o600)
            .context("create runtime artifact transfer package")
    }

    pub fn open_package(&self, writable: bool) -> anyhow::Result<File> {
        let flags = if writable {
            libc::O_RDWR
        } else {
            libc::O_RDONLY
        };
        open_regular_at(&self.file, OsStr::new(PACKAGE_FILE), flags)
            .context("open runtime artifact transfer package")
    }

    pub fn package_len(&self) -> anyhow::Result<u64> {
        Ok(self.open_package(false)?.metadata()?.len())
    }

    pub fn truncate_package(&self, len: u64) -> anyhow::Result<()> {
        let file = self.open_package(true)?;
        file.set_len(len)?;
        file.sync_all()?;
        Ok(())
    }

    pub fn append_package(&self, expected_offset: u64, bytes: &[u8]) -> anyhow::Result<()> {
        let mut file = self.open_package(true)?;
        let len = file.metadata()?.len();
        if len != expected_offset {
            bail!(
                "runtime artifact transfer package length {len} differs from durable prefix {expected_offset}"
            );
        }
        file.seek(SeekFrom::Start(expected_offset))?;
        file.write_all(bytes)?;
        file.sync_data()?;
        Ok(())
    }

    pub fn read_package_range(&self, offset: u64, len: usize) -> anyhow::Result<Vec<u8>> {
        let mut file = self.open_package(false)?;
        let end = offset
            .checked_add(len as u64)
            .context("runtime artifact transfer package range overflow")?;
        if end > file.metadata()?.len() {
            bail!("runtime artifact transfer package range exceeds the durable file");
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0u8; len];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    pub fn remove_package(&self) -> anyhow::Result<()> {
        remove_at_if_present(&self.file, OsStr::new(PACKAGE_FILE))?;
        self.file.sync_all()?;
        Ok(())
    }

    pub fn sync(&self) -> anyhow::Result<()> {
        self.file.sync_all()?;
        Ok(())
    }
}

fn absolute(path: &Path) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn validate_component(value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("transaction directory must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

fn ensure_absolute_directory(path: &Path, mode: libc::mode_t) -> anyhow::Result<File> {
    if !path.is_absolute() {
        bail!("descriptor-relative directory root must be absolute");
    }
    let root = CString::new("/").expect("literal has no NUL");
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open filesystem root");
    }
    let mut current = unsafe { File::from_raw_fd(fd) };
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                bail!("descriptor-relative directory root contains an invalid component")
            }
        };
        ensure_directory_at(&current, name, mode)?;
        current = open_directory_at(&current, name)?;
    }
    Ok(current)
}

fn ensure_directory_at(parent: &File, name: &OsStr, mode: libc::mode_t) -> anyhow::Result<()> {
    match mkdir_at(parent, name, mode) {
        Ok(()) => {
            parent.sync_all()?;
            Ok(())
        }
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
            open_directory_at(parent, name).map(|_| ())
        }
        Err(error) => Err(error.into()),
    }
}

fn mkdir_at(parent: &File, name: &OsStr, mode: libc::mode_t) -> std::io::Result<()> {
    let name = cstring(name)?;
    let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn open_directory_at(parent: &File, name: &OsStr) -> anyhow::Result<File> {
    let name = cstring(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open directory without links");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn create_regular_at(parent: &File, name: &OsStr, mode: libc::mode_t) -> anyhow::Result<File> {
    let name = cstring(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("create regular file without links");
    }
    let file = unsafe { File::from_raw_fd(fd) };
    ensure_regular(&file)?;
    Ok(file)
}

fn open_regular_at(parent: &File, name: &OsStr, flags: libc::c_int) -> anyhow::Result<File> {
    let name = cstring(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open regular file without links");
    }
    let file = unsafe { File::from_raw_fd(fd) };
    ensure_regular(&file)?;
    Ok(file)
}

fn ensure_regular(file: &File) -> anyhow::Result<()> {
    if !file.metadata()?.is_file() {
        bail!("descriptor-relative transfer file is not regular");
    }
    Ok(())
}

fn rename_at(
    old_parent: &File,
    old_name: &OsStr,
    new_parent: &File,
    new_name: &OsStr,
) -> anyhow::Result<()> {
    let old_name = cstring(old_name)?;
    let new_name = cstring(new_name)?;
    let rc = unsafe {
        libc::renameat(
            old_parent.as_raw_fd(),
            old_name.as_ptr(),
            new_parent.as_raw_fd(),
            new_name.as_ptr(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("atomically publish transfer state")
    }
}

fn remove_at_if_present(parent: &File, name: &OsStr) -> anyhow::Result<()> {
    let name = cstring(name)?;
    let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if rc == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(())
    } else {
        Err(error).context("remove transaction-owned transfer file")
    }
}

fn directory_names(directory: &File) -> anyhow::Result<Vec<OsString>> {
    let fd = unsafe { libc::dup(directory.as_raw_fd()) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("duplicate transfer directory");
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(error).context("enumerate transfer directory");
    }
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            unsafe { libc::closedir(stream) };
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name.to_vec()));
        }
    }
    Ok(names)
}

fn cstring(value: &OsStr) -> std::io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "descriptor-relative path component contains NUL",
        )
    })
}
