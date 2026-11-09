use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DATABASE_NAME: &str = "memory.sqlite3";
const PROJECTS_DIR: &str = "projects";
const REGISTRY_DIR: &str = "registry";
const ID_FILE: &str = "kroma-memory-id";
const DATABASE_SIDECARS: &[&str] = &["-wal", "-shm", "-journal"];
/// Publishing an identity is a private temp file plus one no-overwrite link, so
/// a reader rarely has to wait. The retries only cover an older writer that
/// created the file before filling it.
const IDENTITY_READS: usize = 5;
const IDENTITY_RETRY: Duration = Duration::from_millis(20);
const IDENTITY_ATTEMPTS: usize = 3;
/// Nothing regenerates an identity silently: the memories of the old id stay in
/// their own project folder, so recovery must be a deliberate user action.
const IDENTITY_RECOVERY: &str = "a memory id is never regenerated automatically because the \
existing memories stay in projects/<old-id>; restore the id, or move this file aside to start a \
new identity (the old folder is not migrated)";

#[derive(Clone, Debug)]
pub struct Scope {
    pub id: String,
    pub workdir: String,
    pub folder: PathBuf,
    pub database: PathBuf,
}

pub fn resolve(data_dir: &Path, workdir: &Path) -> Result<Scope> {
    if !data_dir.is_absolute() {
        bail!("Data directory must be an absolute path");
    }
    reject_network_path(data_dir, "Data directory")?;
    reject_network_path(workdir, "Workdir")?;

    let canonical = fs::canonicalize(workdir)
        .with_context(|| format!("Workdir is missing or inaccessible: {}", workdir.display()))?;
    if !canonical.is_dir() {
        bail!("Workdir must be a directory");
    }
    reject_network_path(&canonical, "Workdir")?;
    let workdir = canonical
        .to_str()
        .map(str::to_owned)
        .context("Workdir path must be valid UTF-8")?;

    let data_dir = prepare_data_dir(data_dir)?;
    if data_dir == Path::new(&workdir) {
        bail!("Data directory must not be the workdir");
    }

    let id = memory_id(&data_dir, Path::new(&workdir))?;
    let projects = prepare_private_dir(&data_dir.join(PROJECTS_DIR))?;
    let folder = prepare_private_dir(&projects.join(&id))?;

    let database = folder.join(DATABASE_NAME);
    prepare_private_file(&database, true, &folder)?;
    for suffix in DATABASE_SIDECARS {
        prepare_private_file(&sidecar_path(&database, suffix), false, &folder)?;
    }

    Ok(Scope {
        id,
        workdir,
        folder,
        database,
    })
}

/// Stable memory identity. Inside Git it lives in the repository common dir, so
/// every worktree shares it and a clone or a different repository does not.
/// Outside Git, and whenever `.git` cannot be written, a user-level registry
/// keyed by the canonical root holds the very same id.
fn memory_id(data_dir: &Path, workdir: &Path) -> Result<String> {
    let Some(common) = git_common_dir(workdir) else {
        let key = sha256_hex(workdir.to_string_lossy().as_bytes());
        return ensure_identity(&registry_identity_path(data_dir, &key)?, true);
    };
    let repository = common.join(ID_FILE);
    // A readable repository identity always wins, even when `.git` is read-only:
    // reading must never fall back to a different namespace than the one that
    // already holds this workspace's memories.
    if let Some(id) = read_identity(&repository)? {
        mirror_identity(data_dir, &common, &id);
        return Ok(id);
    }
    // A registry entry can be the only surviving identity when `.git` could not
    // be written earlier. Keep that namespace instead of minting a new one.
    let registry =
        registry_identity_path(data_dir, &sha256_hex(common.to_string_lossy().as_bytes()))?;
    if let Some(id) = read_identity(&registry)? {
        // Best effort: a concurrent publisher may already have picked the
        // repository identity, which then wins on the next resolve.
        let _ = publish_identity(&repository, &id);
        // Resolve a concurrent publisher before using a different namespace.
        match read_identity(&repository) {
            Ok(Some(published)) => {
                mirror_identity(data_dir, &common, &published);
                return Ok(published);
            }
            Ok(None) => {}
            Err(error) if is_read_only(&error) => {}
            Err(error) => return Err(error),
        }
        return Ok(id);
    }
    match ensure_identity(&repository, false) {
        Ok(id) => {
            mirror_identity(data_dir, &common, &id);
            Ok(id)
        }
        // A read-only `.git` (packaged or read-only checkout) must not make
        // memory unusable, so the identity moves to the user registry.
        Err(error) if is_read_only(&error) => ensure_identity(&registry, true),
        Err(error) => Err(error),
    }
}

/// Walk up to the nearest `.git` entry. A directory is a repository common
/// dir; a file is a `gitdir:` pointer whose `commondir` file names the common
/// dir shared by linked worktrees.
fn git_common_dir(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let entry = dir.join(".git");
        let Ok(metadata) = fs::symlink_metadata(&entry) else {
            continue;
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            return Some(entry);
        }
        if !metadata.is_file() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&entry) else {
            continue;
        };
        let Some(target) = text
            .lines()
            .find_map(|line| line.trim().strip_prefix("gitdir:"))
        else {
            continue;
        };
        let target = target.trim();
        if target.is_empty() {
            continue;
        }
        let Ok(gitdir) = fs::canonicalize(dir.join(target)) else {
            continue;
        };
        return Some(common_dir_of(&gitdir));
    }
    None
}

/// A linked worktree points at `<common>.git/worktrees/<name>`; the `commondir`
/// file holds the path back to the shared repository dir.
fn common_dir_of(gitdir: &Path) -> PathBuf {
    let Ok(text) = fs::read_to_string(gitdir.join("commondir")) else {
        return gitdir.to_path_buf();
    };
    let target = text.trim();
    if target.is_empty() {
        return gitdir.to_path_buf();
    }
    let common = gitdir.join(target);
    fs::canonicalize(&common).unwrap_or(common)
}

fn registry_identity_path(data_dir: &Path, key: &str) -> Result<PathBuf> {
    let registry = prepare_private_dir(&data_dir.join(REGISTRY_DIR))?;
    Ok(registry.join(format!("{key}.id")))
}

/// Keep the user registry in step with the repository identity so a later
/// read-only `.git` resolves to the same namespace. The registry file is a
/// mirror: memories of an older registry id stay untouched in their own folder.
fn mirror_identity(data_dir: &Path, common: &Path, id: &str) {
    let Ok(path) =
        registry_identity_path(data_dir, &sha256_hex(common.to_string_lossy().as_bytes()))
    else {
        return;
    };
    if matches!(read_identity(&path), Ok(Some(existing)) if existing == id) {
        return;
    }
    let temp = temp_identity_path(&path);
    if write_identity(&temp, id).is_err() {
        return;
    }
    if fs::rename(&temp, &path).is_err() {
        let _ = fs::remove_file(&temp);
        return;
    }
    let _ = restrict_file(&path);
    let _ = verify_private(&path, "Private registry file");
}

/// Read an existing identity or publish a fresh one. Publishing never
/// overwrites, so concurrent first runs converge on a single id.
fn ensure_identity(path: &Path, private: bool) -> Result<String> {
    for _ in 0..IDENTITY_ATTEMPTS {
        if let Some(id) = read_identity(path)? {
            if private {
                restrict_file(path)?;
                verify_private(path, "Private registry file")?;
            }
            return Ok(id);
        }
        let id = random_hex(16);
        match publish_identity(path, &id) {
            Ok(()) => {
                if private {
                    restrict_file(path)?;
                    verify_private(path, "Private registry file")?;
                }
                return Ok(id);
            }
            Err(error) if is_io_kind(&error, &[ErrorKind::AlreadyExists]) => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to create a memory identity: {}", path.display())
                })
            }
        }
    }
    bail!(
        "Failed to create a memory identity: {} was published concurrently too often",
        path.display()
    )
}

/// `Ok(None)` means "not there yet". An existing file is either a usable id or
/// an error: an empty or invalid id is never replaced, because the memories
/// stored under it would be silently orphaned.
fn read_identity(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_link_like(&metadata) || !metadata.is_file() => bail!(
            "Memory identity must be a regular file, not a link: {}. {}",
            path.display(),
            IDENTITY_RECOVERY
        ),
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to inspect memory identity: {}", path.display()))
        }
    }
    for attempt in 0..IDENTITY_READS {
        let text = fs::read_to_string(path)
            .with_context(|| format!("Failed to read memory identity: {}", path.display()))?;
        let id = text.trim();
        if valid_identity(id) {
            return Ok(Some(id.to_string()));
        }
        if !id.is_empty() {
            bail!(
                "Memory identity is invalid: {}. {}",
                path.display(),
                IDENTITY_RECOVERY
            );
        }
        if attempt + 1 < IDENTITY_READS {
            sleep(IDENTITY_RETRY);
        }
    }
    bail!(
        "Memory identity is empty (an interrupted write): {}. {}",
        path.display(),
        IDENTITY_RECOVERY
    )
}

/// Write the id into a private sibling and publish it with a no-overwrite link,
/// so no reader ever observes a half-written identity and no writer ever
/// replaces an existing one.
fn publish_identity(path: &Path, id: &str) -> Result<()> {
    let temp = temp_identity_path(path);
    write_identity(&temp, id)?;
    let published = fs::hard_link(&temp, path);
    let _ = fs::remove_file(&temp);
    match published {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Err(error.into()),
        // Not every filesystem has hard links. A direct create-only write still
        // cannot overwrite an identity, only its visibility window is wider.
        Err(_) => write_identity(path, id),
    }
}

fn write_identity(path: &Path, id: &str) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    use std::io::Write;
    file.write_all(id.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn temp_identity_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(".{ID_FILE}.{}.tmp", random_hex(8)))
}

fn valid_identity(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Random lowercase hex without extra dependencies. `RandomState` is seeded
/// per process from the platform entropy source and re-keyed on every call.
fn random_hex(bytes: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut out = String::with_capacity(bytes * 2);
    while out.len() < bytes * 2 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        hasher.write_i64(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|delta| delta.as_nanos() as i64)
                .unwrap_or(0),
        );
        hasher.write_u32(std::process::id());
        for byte in hasher.finish().to_le_bytes() {
            if out.len() >= bytes * 2 {
                break;
            }
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut id = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(id, "{byte:02x}");
    }
    id
}

fn prepare_data_dir(data_dir: &Path) -> Result<PathBuf> {
    match fs::metadata(data_dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => bail!("Data directory must be a directory: {}", data_dir.display()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir_all(data_dir).with_context(|| {
                format!("Failed to create data directory: {}", data_dir.display())
            })?;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("Failed to inspect data directory: {}", data_dir.display())
            });
        }
    }
    let canonical = fs::canonicalize(data_dir)
        .with_context(|| format!("Failed to resolve data directory: {}", data_dir.display()))?;
    if !canonical.is_dir() {
        bail!("Data directory must be a directory: {}", data_dir.display());
    }
    reject_network_path(&canonical, "Data directory")?;
    Ok(canonical)
}

/// Prepare a private directory and return its real path. A symlink or junction
/// is a deliberate relocation (the data directory itself is allowed to be one),
/// so it is followed and the real target is validated and restricted instead.
fn prepare_private_dir(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => create_private_dir(path)?,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("Failed to inspect private directory: {}", path.display())
            });
        }
    }
    let real = fs::canonicalize(path).with_context(|| {
        format!(
            "Failed to resolve private directory (it may be a broken link): {}",
            path.display()
        )
    })?;
    let metadata = fs::metadata(&real)
        .with_context(|| format!("Failed to inspect private directory: {}", real.display()))?;
    if !metadata.is_dir() {
        bail!(
            "Private storage path must be a directory: {}",
            path.display()
        );
    }
    reject_network_path(&real, "Private storage directory")?;
    secure(&real, true)?;
    verify_private(&real, "Private storage directory")?;
    Ok(real)
}

fn create_private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = DirBuilder::new();
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("Failed to create private directory: {}", path.display())),
    }
}

/// Prepare a private file the workspace writes itself. Files are never
/// followed: a link here would move the database or a sidecar outside the
/// project folder, where its SQLite writes would not be private.
fn prepare_private_file(path: &Path, create: bool, folder: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_file_metadata(path, &metadata)?,
        Err(error) if error.kind() == ErrorKind::NotFound && !create => return Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => create_private_file(path)?,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to inspect private database path: {}",
                    path.display()
                )
            });
        }
    }
    let real = fs::canonicalize(path).with_context(|| {
        format!(
            "Failed to resolve private storage file (it may be a broken link): {}",
            path.display()
        )
    })?;
    if real.parent() != Some(folder) {
        bail!(
            "Private storage file must stay inside its project folder: {}",
            path.display()
        );
    }
    secure(&real, false)?;
    verify_private(&real, "Private storage file")?;
    Ok(())
}

fn create_private_file(path: &Path) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            validate_file_metadata(path, &fs::symlink_metadata(path)?)
        }
        Err(error) => Err(error)
            .with_context(|| format!("Failed to create private database: {}", path.display())),
    }
}

fn validate_file_metadata(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if is_link_like(metadata) {
        bail!(
            "Private storage file must not be a symlink or reparse point: {}",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "Private storage path must be a regular file: {}",
            path.display()
        );
    }
    Ok(())
}

fn sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

fn is_link_like(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    false
}

fn is_io_kind(error: &anyhow::Error, kinds: &[ErrorKind]) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| kinds.contains(&io.kind()))
}

fn is_read_only(error: &anyhow::Error) -> bool {
    is_io_kind(
        error,
        &[ErrorKind::PermissionDenied, ErrorKind::ReadOnlyFilesystem],
    )
}

/// Apply private permissions every time. Another process, an archive extraction
/// or a restore can widen a mode behind this process, so a per-process "already
/// done" cache would trust a mode that is no longer set.
fn secure(path: &Path, directory: bool) -> Result<()> {
    if directory {
        restrict_directory(path)?;
    } else {
        restrict_file(path)?;
    }
    Ok(())
}

/// Confirm the permissions just applied are really in force on the resolved
/// target, so a filesystem that ignores modes is not silently trusted.
#[cfg(unix)]
fn verify_private(path: &Path, what: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .with_context(|| format!("Failed to verify {what}: {}", path.display()))?
        .permissions()
        .mode()
        & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{what} permissions could not be made private: {} is 0{mode:03o}",
            path.display()
        );
    }
    Ok(())
}

/// Windows private storage is an explicit protected DACL; a failed
/// `SetNamedSecurityInfoW` is already an error, so there is nothing to re-read.
#[cfg(not(unix))]
fn verify_private(_path: &Path, _what: &str) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("Failed to restrict private directory: {}", path.display()))
}

#[cfg(windows)]
fn restrict_directory(path: &Path) -> Result<()> {
    windows_private::restrict(path, true)
}

#[cfg(not(any(unix, windows)))]
fn restrict_directory(_path: &Path) -> Result<()> {
    bail!("Private storage permissions are unsupported on this platform")
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Failed to restrict private database: {}", path.display()))
}

#[cfg(windows)]
fn restrict_file(path: &Path) -> Result<()> {
    windows_private::restrict(path, false)
}

#[cfg(not(any(unix, windows)))]
fn restrict_file(_path: &Path) -> Result<()> {
    bail!("Private storage permissions are unsupported on this platform")
}

#[cfg(windows)]
fn reject_network_path(path: &Path, what: &str) -> Result<()> {
    let bs = char::from_u32(0x5C).expect("ASCII backslash");
    let double = format!("{bs}{bs}");
    let verbatim = format!("{double}?{bs}");
    let unc = format!("unc{bs}");
    let nt = format!("{bs}??{bs}");
    let device = format!("{double}.{bs}");
    let text = path
        .as_os_str()
        .to_string_lossy()
        .trim()
        .to_ascii_lowercase();
    let tail = text
        .strip_prefix(&verbatim)
        .or_else(|| text.strip_prefix("//?/"));
    let is_network = if let Some(rest) = tail {
        let bytes = rest.as_bytes();
        let is_drive = bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == 0x5C || bytes[2] == b'/');
        !is_drive || rest.starts_with(&unc) || rest.starts_with("unc/")
    } else {
        text.starts_with(&double)
            || text.starts_with("//")
            || text.starts_with(&nt)
            || text.starts_with("//??/")
            || text.starts_with(&device)
            || text.starts_with("//./")
    };
    if is_network {
        bail!("{what} must be a local path, not a network or device path");
    }
    Ok(())
}

#[cfg(unix)]
fn reject_network_path(path: &Path, what: &str) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    if path.as_os_str().as_bytes().starts_with(b"//") {
        bail!("{what} must be a local path, not a network path");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn reject_network_path(_path: &Path, _what: &str) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
mod windows_private {
    use super::*;
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows_sys::Win32::Security::Authorization::{
        SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
        NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, GetLengthSid, GetTokenInformation, TokenUser, WinLocalSystemSid, ACL,
        DACL_SECURITY_INFORMATION, NO_INHERITANCE, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
        SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const FILE_ALL_ACCESS: u32 = 0x001F_01FF;

    pub(super) fn restrict(path: &Path, directory: bool) -> Result<()> {
        let user = current_user_sid()?;
        let mut system = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut system_len = SECURITY_MAX_SID_SIZE;
        let created = unsafe {
            CreateWellKnownSid(
                WinLocalSystemSid,
                null_mut(),
                system.as_mut_ptr() as PSID,
                &mut system_len,
            )
        };
        if created == 0 {
            bail!("Failed to resolve the local SYSTEM account for private storage");
        }
        system.truncate(system_len as usize);

        let inheritance = if directory {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            NO_INHERITANCE
        };
        let entries = [
            explicit_access(&user, inheritance),
            explicit_access(&system, inheritance),
        ];
        let mut acl: *mut ACL = null_mut();
        let result =
            unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), null(), &mut acl) };
        if result != 0 {
            bail!("Failed to build private storage permissions (Windows error {result})");
        }
        let acl = AclGuard(acl);

        let target: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
        let result = unsafe {
            SetNamedSecurityInfoW(
                target.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                acl.0,
                null(),
            )
        };
        if result != 0 {
            bail!("Failed to apply private storage permissions (Windows error {result})");
        }
        Ok(())
    }

    fn explicit_access(sid: &[u8], inheritance: u32) -> EXPLICIT_ACCESS_W {
        EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: inheritance,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_USER,
                ptstrName: sid.as_ptr() as *mut u16,
            },
        }
    }

    fn current_user_sid() -> Result<Vec<u8>> {
        let mut token: HANDLE = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            bail!("Failed to open the current process token for private storage");
        }
        let result = read_user_sid(token);
        unsafe {
            CloseHandle(token);
        }
        result
    }

    fn read_user_sid(token: HANDLE) -> Result<Vec<u8>> {
        let mut needed = 0u32;
        unsafe {
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed);
        }
        if needed == 0 {
            bail!("Failed to size the current process user token");
        }
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
        let mut written = needed;
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr() as *mut _,
                needed,
                &mut written,
            )
        };
        if ok == 0 || written < std::mem::size_of::<TOKEN_USER>() as u32 {
            bail!("Failed to read the current process user token");
        }
        let token_user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
        let sid: SID_AND_ATTRIBUTES = token_user.User;
        let len = unsafe { GetLengthSid(sid.Sid) } as usize;
        if len == 0 || len > SECURITY_MAX_SID_SIZE as usize {
            bail!("The current process user SID is not usable");
        }
        Ok(unsafe { std::slice::from_raw_parts(sid.Sid as *const u8, len) }.to_vec())
    }

    struct AclGuard(*mut ACL);

    impl Drop for AclGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LocalFree(self.0 as HLOCAL);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    struct Fixture {
        root: TempDir,
        data: PathBuf,
        work: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let data = root.path().join("data");
            let work = root.path().join("work");
            fs::create_dir(&work).unwrap();
            Self { root, data, work }
        }

        fn canonical_data(&self) -> PathBuf {
            fs::canonicalize(&self.data).unwrap()
        }
    }

    fn git_repo(root: &Path, name: &str) -> PathBuf {
        let repo = root.join(name);
        fs::create_dir_all(repo.join(".git")).unwrap();
        repo
    }

    fn git_worktree(root: &Path, repo: &Path, name: &str) -> PathBuf {
        let worktree = root.join(name);
        fs::create_dir_all(&worktree).unwrap();
        let admin = repo.join(".git").join("worktrees").join(name);
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        worktree
    }

    fn registry_identity(data: &Path, key_source: &Path) -> PathBuf {
        let key = sha256_hex(key_source.to_string_lossy().as_bytes());
        data.join(REGISTRY_DIR).join(format!("{key}.id"))
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn identity_uses_a_user_registry_outside_git() {
        let fixture = Fixture::new();
        let scope = resolve(&fixture.data, &fixture.work).unwrap();
        let canonical = fs::canonicalize(&fixture.work).unwrap();
        assert_eq!(scope.workdir, canonical.to_str().unwrap());
        assert_eq!(scope.id.len(), 32);
        assert!(scope.id.chars().all(|item| item.is_ascii_hexdigit()));
        let data = fixture.canonical_data();
        assert_eq!(scope.folder, data.join(PROJECTS_DIR).join(&scope.id));
        assert_eq!(scope.database, scope.folder.join(DATABASE_NAME));

        let registry = registry_identity(&data, Path::new(&scope.workdir));
        assert_eq!(fs::read_to_string(&registry).unwrap().trim(), scope.id);

        let same = resolve(&fixture.data, &fixture.work.join(".")).unwrap();
        assert_eq!(same.id, scope.id);

        let other = fixture.root.path().join("other");
        fs::create_dir(&other).unwrap();
        assert_ne!(resolve(&fixture.data, &other).unwrap().id, scope.id);
    }

    #[test]
    fn git_worktrees_share_identity_and_repositories_stay_isolated() {
        let fixture = Fixture::new();
        let repo = git_repo(fixture.root.path(), "repo");
        let worktree = git_worktree(fixture.root.path(), &repo, "wt");
        let nested = worktree.join("src");
        fs::create_dir(&nested).unwrap();

        let main = resolve(&fixture.data, &repo).unwrap();
        assert_eq!(resolve(&fixture.data, &worktree).unwrap().id, main.id);
        assert_eq!(resolve(&fixture.data, &nested).unwrap().id, main.id);
        assert_eq!(
            fs::read_to_string(repo.join(".git").join(ID_FILE))
                .unwrap()
                .trim(),
            main.id
        );

        let moved = fixture.root.path().join("moved");
        fs::rename(&repo, &moved).unwrap();
        assert_eq!(resolve(&fixture.data, &moved).unwrap().id, main.id);

        let clone = git_repo(fixture.root.path(), "clone");
        assert_ne!(resolve(&fixture.data, &clone).unwrap().id, main.id);
    }

    #[test]
    fn the_repository_identity_is_mirrored_into_the_user_registry() {
        let fixture = Fixture::new();
        let repo = git_repo(fixture.root.path(), "repo");
        let scope = resolve(&fixture.data, &repo).unwrap();
        let common = fs::canonicalize(repo.join(".git")).unwrap();
        let mirror = registry_identity(&fixture.canonical_data(), &common);
        assert_eq!(fs::read_to_string(&mirror).unwrap().trim(), scope.id);
        assert_eq!(
            fs::read_to_string(repo.join(".git").join(ID_FILE))
                .unwrap()
                .trim(),
            scope.id
        );
    }

    #[test]
    fn rejects_relative_data_dir() {
        let fixture = Fixture::new();
        assert!(resolve(Path::new("relative-data"), &fixture.work).is_err());
    }

    #[test]
    fn private_storage_is_created_and_reused() {
        let fixture = Fixture::new();
        let first = resolve(&fixture.data, &fixture.work).unwrap();
        assert!(first.folder.is_dir());
        assert_eq!(fs::metadata(&first.database).unwrap().len(), 0);
        let second = resolve(&fixture.data, &fixture.work).unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.folder, first.folder);
        assert_eq!(second.database, first.database);
    }

    #[cfg(unix)]
    #[test]
    fn private_storage_uses_0700_and_0600() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        fs::create_dir_all(&fixture.data).unwrap();
        fs::set_permissions(&fixture.data, fs::Permissions::from_mode(0o755)).unwrap();
        let scope = resolve(&fixture.data, &fixture.work).unwrap();
        let registry = fixture.canonical_data().join(REGISTRY_DIR);
        let identity = registry_identity(&fixture.canonical_data(), Path::new(&scope.workdir));
        assert_eq!(mode(&fixture.data), 0o755, "the data root stays user-owned");
        assert_eq!(mode(&registry), 0o700);
        assert_eq!(mode(&identity), 0o600);
        assert_eq!(mode(scope.folder.parent().unwrap()), 0o700);
        assert_eq!(mode(&scope.folder), 0o700);
        assert_eq!(mode(&scope.database), 0o600);

        fs::set_permissions(
            scope.folder.parent().unwrap(),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::set_permissions(&scope.folder, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&scope.database, fs::Permissions::from_mode(0o644)).unwrap();
        resolve(&fixture.data, &fixture.work).unwrap();
        assert_eq!(mode(scope.folder.parent().unwrap()), 0o700);
        assert_eq!(mode(&scope.folder), 0o700);
        assert_eq!(mode(&scope.database), 0o600);
    }

    #[test]
    fn rejects_projects_path_that_is_not_a_directory() {
        let fixture = Fixture::new();
        fs::create_dir_all(&fixture.data).unwrap();
        fs::write(fixture.data.join(PROJECTS_DIR), b"not a directory").unwrap();
        assert!(resolve(&fixture.data, &fixture.work).is_err());
    }

    #[test]
    fn a_relocated_projects_directory_is_followed_and_locked_down() {
        let fixture = Fixture::new();
        let elsewhere = fixture.root.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::create_dir_all(&fixture.data).unwrap();
        if !dir_symlink(&elsewhere, &fixture.data.join(PROJECTS_DIR)) {
            return;
        }
        let scope = resolve(&fixture.data, &fixture.work).unwrap();
        let real = fs::canonicalize(&elsewhere).unwrap();
        assert_eq!(scope.folder, real.join(&scope.id));
        assert_eq!(scope.database, real.join(&scope.id).join(DATABASE_NAME));
        assert!(scope.database.is_file());
        #[cfg(unix)]
        assert_eq!(mode(&real), 0o700, "the relocation target is locked down");
        let same = resolve(&fixture.data, &fixture.work).unwrap();
        assert_eq!(same.id, scope.id);
        assert_eq!(same.folder, scope.folder);
    }

    #[test]
    fn a_relocated_project_folder_is_followed_and_stays_contained() {
        let fixture = Fixture::new();
        let seed = resolve(&fixture.data, &fixture.work).unwrap();
        let elsewhere = fixture.root.path().join("project-elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        fs::remove_dir_all(&seed.folder).unwrap();
        if !dir_symlink(&elsewhere, &seed.folder) {
            return;
        }
        let scope = resolve(&fixture.data, &fixture.work).unwrap();
        let real = fs::canonicalize(&elsewhere).unwrap();
        assert_eq!(scope.id, seed.id);
        assert_eq!(scope.folder, real);
        assert_eq!(scope.database, real.join(DATABASE_NAME));
        assert!(scope.database.is_file());
        #[cfg(unix)]
        assert_eq!(mode(&real), 0o700);
    }

    #[test]
    fn rejects_symlinked_database() {
        let fixture = Fixture::new();
        let scope = resolve(&fixture.data, &fixture.work).unwrap();
        let outside = fixture.root.path().join("outside.db");
        fs::write(&outside, b"outside").unwrap();
        fs::remove_file(&scope.database).unwrap();
        if !file_symlink(&outside, &scope.database) {
            return;
        }
        assert!(resolve(&fixture.data, &fixture.work).is_err());
    }

    #[test]
    fn rejects_symlinked_wal_sidecar() {
        let fixture = Fixture::new();
        let scope = resolve(&fixture.data, &fixture.work).unwrap();
        let outside = fixture.root.path().join("outside.db");
        fs::write(&outside, b"outside").unwrap();
        let wal = sidecar_path(&scope.database, "-wal");
        if !file_symlink(&outside, &wal) {
            return;
        }
        assert!(resolve(&fixture.data, &fixture.work).is_err());
    }

    /// A portable build keeps its data next to the binary. Only the private
    /// subdirectories need to be locked down, so that location is allowed.
    #[test]
    fn data_next_to_the_executable_is_allowed() {
        let Ok(executable) = std::env::current_exe() else {
            return;
        };
        let Some(parent) = executable.parent() else {
            return;
        };
        let root = parent.join(format!("kroma-portable-{}", random_hex(6)));
        let work = root.join("work");
        if fs::create_dir_all(&work).is_err() {
            return;
        }
        // Keep the identity inside the fixture instead of the enclosing repo.
        fs::create_dir_all(root.join(".git")).unwrap();
        let data = root.join("data");
        let scope = resolve(&data, &work).unwrap();
        assert_eq!(
            scope.database,
            fs::canonicalize(&data)
                .unwrap()
                .join(PROJECTS_DIR)
                .join(&scope.id)
                .join(DATABASE_NAME)
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn concurrent_first_resolves_converge_on_one_identity() {
        let fixture = Fixture::new();
        let data = fixture.data.clone();
        let work = fixture.work.clone();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let data = data.clone();
                let work = work.clone();
                std::thread::spawn(move || resolve(&data, &work).map(|scope| scope.id))
            })
            .collect();
        let ids: Vec<String> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap())
            .collect();
        assert!(
            ids.iter().all(|id| *id == ids[0]),
            "concurrent first runs must converge: {ids:?}"
        );
        assert!(valid_identity(&ids[0]));
        let names: Vec<String> = fs::read_dir(fixture.canonical_data().join(REGISTRY_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names.len(),
            1,
            "one identity file and no leaked temp files: {names:?}"
        );
        assert!(names[0].ends_with(".id"));
    }

    #[test]
    fn an_invalid_repository_identity_is_reported_and_not_replaced() {
        let fixture = Fixture::new();
        let repo = git_repo(fixture.root.path(), "repo");
        fs::write(repo.join(".git").join(ID_FILE), b"zz\n").unwrap();
        let error = resolve(&fixture.data, &repo).unwrap_err().to_string();
        assert!(error.contains("Memory identity is invalid"), "{error}");
        assert!(error.contains("never regenerated"), "{error}");
        assert_eq!(
            fs::read_to_string(repo.join(".git").join(ID_FILE)).unwrap(),
            "zz\n"
        );
        assert!(
            !fixture.canonical_data().join(PROJECTS_DIR).exists(),
            "an unusable identity must not publish storage"
        );
    }

    #[test]
    fn an_invalid_registry_identity_is_reported_and_not_replaced() {
        let fixture = Fixture::new();
        let scope = resolve(&fixture.data, &fixture.work).unwrap();
        let path = registry_identity(&fixture.canonical_data(), Path::new(&scope.workdir));
        fs::write(&path, b"not a memory id\n").unwrap();
        let error = resolve(&fixture.data, &fixture.work)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Memory identity is invalid"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "not a memory id\n");
    }

    #[test]
    fn an_empty_identity_is_reported_and_not_replaced() {
        let fixture = Fixture::new();
        let scope = resolve(&fixture.data, &fixture.work).unwrap();
        let path = registry_identity(&fixture.canonical_data(), Path::new(&scope.workdir));
        fs::write(&path, b"").unwrap();
        let error = resolve(&fixture.data, &fixture.work)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Memory identity is empty"), "{error}");
        assert!(
            error.contains(&path.display().to_string()),
            "the error names the file: {error}"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            b"",
            "an interrupted identity is never regenerated"
        );
    }

    /// A read-only `.git` (packaged or read-only checkout) must keep one stable
    /// identity in the user registry and adopt it back once the repo is writable.
    #[cfg(unix)]
    #[test]
    fn a_read_only_git_dir_falls_back_to_the_registry_and_keeps_one_identity() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        let repo = git_repo(fixture.root.path(), "repo");
        let git = repo.join(".git");
        fs::set_permissions(&git, fs::Permissions::from_mode(0o500)).unwrap();
        // Root ignores directory modes, so probe instead of trusting the mode.
        if fs::write(git.join("probe"), b"").is_ok() {
            let _ = fs::remove_file(git.join("probe"));
            fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let first = resolve(&fixture.data, &repo).unwrap();
        let second = resolve(&fixture.data, &repo).unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(second.id, first.id, "a read-only .git keeps one identity");
        let registry =
            registry_identity(&fixture.canonical_data(), &fs::canonicalize(&git).unwrap());
        assert_eq!(fs::read_to_string(&registry).unwrap().trim(), first.id);
        assert!(
            !git.join(ID_FILE).exists(),
            "a read-only .git is not written"
        );

        let third = resolve(&fixture.data, &repo).unwrap();
        assert_eq!(third.id, first.id, "the repository adopts the registry id");
        assert_eq!(
            fs::read_to_string(git.join(ID_FILE)).unwrap().trim(),
            first.id
        );
    }

    #[cfg(unix)]
    fn dir_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    fn dir_symlink(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
    }

    #[cfg(unix)]
    fn file_symlink(target: &Path, link: &Path) -> bool {
        std::os::unix::fs::symlink(target, link).is_ok()
    }

    #[cfg(windows)]
    fn file_symlink(target: &Path, link: &Path) -> bool {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }

    #[cfg(windows)]
    #[test]
    fn windows_dacl_grants_current_user_and_system() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};
        use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{
            GetAce, ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
            INHERITED_ACE, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR,
        };

        fn ace_flags(path: &Path) -> Vec<u8> {
            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let result = unsafe {
                GetNamedSecurityInfoW(
                    wide.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut descriptor,
                )
            };
            assert_eq!(result, 0);
            let mut flags = Vec::new();
            if !dacl.is_null() {
                for index in 0..unsafe { (*dacl).AceCount } {
                    let mut ace: *mut std::ffi::c_void = std::ptr::null_mut();
                    if unsafe { GetAce(dacl, index as u32, &mut ace) } != 0 {
                        flags.push(unsafe { (*(ace as *const ACE_HEADER)).AceFlags });
                    }
                }
            }
            unsafe {
                LocalFree(descriptor as HLOCAL);
            }
            flags
        }

        let fixture = Fixture::new();
        let scope = resolve(&fixture.data, &fixture.work).unwrap();
        let inheritable = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;

        let directory_flags = ace_flags(&scope.folder);
        assert_eq!(directory_flags.len(), 2);
        for flags in directory_flags {
            assert_eq!(flags & INHERITED_ACE as u8, 0);
            assert_eq!(flags & inheritable, inheritable);
        }

        let file_flags = ace_flags(&scope.database);
        assert_eq!(file_flags.len(), 2);
        for flags in file_flags {
            assert_eq!(flags & INHERITED_ACE as u8, 0);
            assert_eq!(flags & inheritable, 0);
        }
    }
}
