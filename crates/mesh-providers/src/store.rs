//! On-disk account storage.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use mesh_core::AccountId;

use crate::{ProviderError, StoredAccount};

/// The default data directory of the daemon and `meshctl`: `mesh` in the
/// per-machine user data directory (`$XDG_DATA_HOME` or `~/.local/share` on
/// Linux, `%LOCALAPPDATA%` on Windows). It is never the roaming profile on
/// Windows, so the tokens stay on this machine.
pub fn default_data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|dir| dir.join("mesh"))
}

/// Stores one JSON file per account, `<dir>/<account id>.json`, written
/// atomically (temp file + rename).
///
/// On Unix the files have mode 0600 and the directory mode 0700. Existing
/// account files and the store directory must give no access to group or
/// other users. The store and its ancestors must be controlled by the user. Windows has
/// no such mode, and the store sets no ACL of its own: it relies on the ACL
/// that the directory inherits. Under the user profile, which is where
/// [`default_data_dir`] points, that ACL grants access only to the user,
/// `SYSTEM`, and the Administrators. A `data_dir` outside the profile, such
/// as one under `C:\ProgramData`, may be readable by other users, so keep
/// the tokens in the profile.
#[derive(Clone, Debug)]
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    /// Create a store rooted at `dir`. The directory is created on the first save.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        TokenStore { dir: dir.into() }
    }

    /// The directory containing the account files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Load every account in the directory, sorted by id.
    ///
    /// A missing directory is an empty store. Files that do not end in
    /// `.json` are ignored. A file that cannot be read as an account, or
    /// whose name does not match its account id, is logged with
    /// `tracing::warn` and skipped.
    pub fn load_all(&self) -> Result<Vec<StoredAccount>, ProviderError> {
        match check_path(&self.dir, true) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };

        let mut accounts = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") || !entry.file_type()?.is_file() {
                continue;
            }
            match read_account(&path) {
                Ok(account) => accounts.push(account),
                Err(error) => match error {
                    ProviderError::Json(error) => tracing::warn!(
                        path = %path.display(), line = error.line(), column = error.column(),
                        "skipping invalid account JSON"
                    ),
                    error => tracing::warn!(
                        path = %path.display(), %error, "skipping unreadable account file"
                    ),
                },
            }
        }
        accounts.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(accounts)
    }

    /// Write `account`, replacing any earlier file for the same id. On Unix
    /// the directory is created with mode 0700 when it does not exist.
    pub fn save(&self, account: &StoredAccount) -> Result<(), ProviderError> {
        create_private_dir(&self.dir)?;
        check_path(&self.dir, true)?;
        let json = serde_json::to_vec_pretty(account)?;

        let nonce: u64 = rand::random();
        let temp = self.dir.join(format!(".{}.{nonce:016x}.tmp", account.id));
        let mut file = create_private_file(&temp)?;
        let written = file.write_all(&json).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = written.and_then(|()| fs::rename(&temp, self.path_for(&account.id))) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        if let Ok(dir) = fs::File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// Load one account. Returns `None` when its file or the store is absent.
    pub fn load(&self, id: &AccountId) -> Result<Option<StoredAccount>, ProviderError> {
        match check_path(&self.dir, true)
            .map_err(ProviderError::from)
            .and_then(|()| read_account(&self.path_for(id)))
        {
            Ok(account) => Ok(Some(account)),
            Err(ProviderError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Delete the file for `id`. Removing an account that is not stored is not an error.
    pub fn remove(&self, id: &AccountId) -> Result<(), ProviderError> {
        match check_path(&self.dir, true).and_then(|()| fs::remove_file(self.path_for(id))) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn path_for(&self, id: &AccountId) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
}

fn read_account(path: &Path) -> Result<StoredAccount, ProviderError> {
    check_path(path, false)?;
    use std::io::Read;
    const MAX_ACCOUNT_BYTES: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_ACCOUNT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ACCOUNT_BYTES {
        return Err(ProviderError::Other("account file is too large".to_owned()));
    }
    let account: StoredAccount = serde_json::from_slice(&bytes)?;
    let stem = path.file_stem().and_then(|stem| stem.to_str());
    if stem != Some(account.id.as_str()) {
        return Err(ProviderError::Other(
            "file name does not match account id".to_owned(),
        ));
    }
    Ok(account)
}

pub(crate) fn check_path(path: &Path, directory: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !(if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    }) {
        return Err(io::Error::other(
            "token store requires regular files and directories",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "token store grants access to other users",
            ));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & REPARSE_POINT != 0 {
            return Err(io::Error::other("token store must not use reparse points"));
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

pub(crate) fn create_private_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
