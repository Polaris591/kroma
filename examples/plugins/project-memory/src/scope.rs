use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DATABASE_NAME: &str = "memory.sqlite3";
const PROJECTS_DIR: &str = "projects";
const REGISTRY_DIR: &str = "registry";
const ID_FILE: &str = "kroma-memory-id";
const DATABASE_SIDECARS: &[&str] = &["-wal", "-shm", "-journal"];

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
    reject_executable_dir(&data_dir)?;

    let id = memory_id(&data_dir, Path::new(&workdir))?;
    let projects = data_dir.join(PROJECTS_DIR);
    let folder = projects.join(&id);
    prepare_private_dir(&projects)?;
    prepare_private_dir(&folder)?;

    let database = folder.join(DATABASE_NAME);
    prepare_private_file(&database, true)?;
    for suffix in DATABASE_SIDECARS {
        prepare_private_file(&sidecar_path(&database, suffix), false)?;
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
/// Outside Git a user-level registry keyed by the canonical workdir is used.
fn memory_id(data_dir: &Path, workdir: &Path) -> Result<String> {
    match git_common_dir(workdir) {
        Some(common) => read_or_create_id(&common.join(ID_FILE), false),
        None => {
            let registry = data_dir.join(REGISTRY_DIR);
            prepare_private_dir(&registry)?;
            let key = sha256_hex(workdir.to_string_lossy().as_bytes());
            read_or_create_id(&registry.join(format!("{key}.id")), true)
        }
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

/// Read an existing identity file or create one atomically. `create_new` makes
/// concurrent first runs converge on a single id instead of overwriting it.
fn read_or_create_id(path: &Path, private: bool) -> Result<String> {
    for _ in 0..2 {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                bail!("Memory identity path is not a regular file")
            }
            Ok(_) => {
                let text = fs::read_to_string(path)
                    .with_context(|| format!("Failed to read {}", path.display()))?;
                let id = text.trim().to_string();
                if !valid_identity(&id) {
                    bail!("Memory identity file is invalid");
                }
                return Ok(id);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to inspect {}", path.display()))
            }
        }
        let id = random_hex(16);
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(id.as_bytes())?;
                file.write_all(b"\n")?;
                file.sync_all()?;
                drop(file);
                if private {
                    let _ = restrict_file(path);
                }
                return Ok(id);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to create {}", path.display()))
            }
        }
    }
    bail!("Failed to create a memory identity")
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

fn reject_executable_dir(data_dir: &Path) -> Result<()> {
    let Ok(executable) = std::env::current_exe() else {
        return Ok(());
    };
    let Ok(executable) = fs::canonicalize(executable) else {
        return Ok(());
    };
    let Some(executable_dir) = executable.parent() else {
        return Ok(());
    };
    if data_dir.starts_with(executable_dir) {
        bail!("Data directory must not be inside the executable directory");
    }
    Ok(())
}

fn prepare_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_dir_metadata(path, &metadata)?,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            #[cfg(unix)]
            let mut builder = DirBuilder::new();
            #[cfg(not(unix))]
            let builder = DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    validate_dir_metadata(path, &fs::symlink_metadata(path)?)?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Failed to create private directory: {}", path.display())
                    });
                }
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("Failed to inspect private directory: {}", path.display())
            });
        }
    }
    restrict_directory(path)
}

fn validate_dir_metadata(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if is_link_like(metadata) {
        bail!(
            "Private storage directory must not be a symlink or reparse point: {}",
            path.display()
        );
    }
    if !metadata.is_dir() {
        bail!(
            "Private storage path must be a directory: {}",
            path.display()
        );
    }
    Ok(())
}

fn prepare_private_file(path: &Path, create: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_file_metadata(path, &metadata)?,
        Err(error) if error.kind() == ErrorKind::NotFound && !create => return Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(path) {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    validate_file_metadata(path, &fs::symlink_metadata(path)?)?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Failed to create private database: {}", path.display())
                    });
                }
            }
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Failed to inspect private database path: {}",
                    path.display()
                )
            });
        }
    }
    restrict_file(path)
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

    #[test]
    fn identity_uses_a_user_registry_outside_git() {
        let fixture = Fixture::new();
        let scope = resolve(&fixture.data, &fixture.work).unwrap();
        let canonical = fs::canonicalize(&fixture.work).unwrap();
        assert_eq!(scope.workdir, canonical.to_str().unwrap());
        assert_eq!(scope.id.len(), 32);
        assert!(scope.id.chars().all(|item| item.is_ascii_hexdigit()));
        let data = fs::canonicalize(&fixture.data).unwrap();
        assert_eq!(scope.folder, data.join("projects").join(&scope.id));
        assert_eq!(scope.database, scope.folder.join("memory.sqlite3"));

        let key = sha256_hex(scope.workdir.as_bytes());
        let registry = data.join("registry").join(format!("{key}.id"));
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

        fn mode(path: &Path) -> u32 {
            fs::metadata(path).unwrap().permissions().mode() & 0o777
        }

        let fixture = Fixture::new();
        fs::create_dir_all(&fixture.data).unwrap();
        fs::set_permissions(&fixture.data, fs::Permissions::from_mode(0o755)).unwrap();
        let scope = resolve(&fixture.data, &fixture.work).unwrap();
        assert_eq!(mode(&fixture.data), 0o755);
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
        fs::write(fixture.data.join("projects"), b"not a directory").unwrap();
        assert!(resolve(&fixture.data, &fixture.work).is_err());
    }

    #[test]
    fn rejects_symlinked_projects_directory() {
        let fixture = Fixture::new();
        let outside = fixture.root.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::create_dir_all(&fixture.data).unwrap();
        if !dir_symlink(&outside, &fixture.data.join("projects")) {
            return;
        }
        assert!(resolve(&fixture.data, &fixture.work).is_err());
    }

    #[test]
    fn rejects_symlinked_project_directory() {
        let fixture = Fixture::new();
        let seed = resolve(&fixture.data, &fixture.work).unwrap();
        let outside = fixture.root.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::create_dir_all(fixture.data.join("projects")).unwrap();
        if !dir_symlink(&outside, &fixture.data.join("projects").join(&seed.id)) {
            return;
        }
        assert!(resolve(&fixture.data, &fixture.work).is_err());
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
