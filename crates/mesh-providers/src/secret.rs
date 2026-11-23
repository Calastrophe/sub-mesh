//! A random secret kept in a private file next to the account tokens.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRngCore;
use rand::rngs::OsRng;

use crate::store::{check_path, create_private_dir, create_private_file};

const SECRET_BYTES: usize = 32;
const SECRET_CHARS: usize = 43;

/// Read the secret in `path`, or create it from the operating system's
/// random number generator when the file is missing.
///
/// The secret is 256 bits as 43 URL-safe base64 characters. The file is
/// written with the same protections as account files (see
/// [`TokenStore`](crate::TokenStore)): mode 0600 on Unix, no links or
/// reparse points, and no group or other access. A missing directory is
/// created with mode 0700. An existing directory may be open for reading but
/// not for writing by others. The file appears whole or not at all, and an
/// existing file is never replaced.
pub fn read_or_create_secret(path: &Path) -> io::Result<String> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
        check_dir(dir)?;
    }
    match read_secret(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        result => return result,
    }

    let mut bytes = [0u8; SECRET_BYTES];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|error| io::Error::other(format!("no system randomness: {error}")))?;
    let secret = URL_SAFE_NO_PAD.encode(bytes);

    let nonce: u64 = rand::random();
    let temp = path.with_extension(format!("{nonce:016x}.tmp"));
    let mut file = create_private_file(&temp)?;
    let written = file
        .write_all(secret.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    let linked = written.and_then(|()| fs::hard_link(&temp, path));
    let _ = fs::remove_file(&temp);
    match linked {
        Ok(()) => Ok(secret),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_secret(path),
        Err(error) => Err(error),
    }
}

/// Read a secret written by [`read_or_create_secret`]. A missing file is an
/// [`io::ErrorKind::NotFound`] error. A file that is a link, is open to
/// other users on Unix, or does not hold a secret is an error.
pub fn read_secret(path: &Path) -> io::Result<String> {
    check_path(path, false)?;
    let mut text = String::new();
    fs::File::open(path)?
        .take(SECRET_CHARS as u64 + 1)
        .read_to_string(&mut text)?;
    let text = text.trim_end();
    if text.len() == SECRET_CHARS
        && URL_SAFE_NO_PAD
            .decode(text)
            .is_ok_and(|b| b.len() == SECRET_BYTES)
    {
        Ok(text.to_owned())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file does not hold a 256-bit secret",
        ))
    }
}

fn check_dir(dir: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(
            "secret directory must be a real directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "secret directory is writable by other users",
            ));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & REPARSE_POINT != 0 {
            return Err(io::Error::other(
                "secret directory must not be a reparse point",
            ));
        }
    }
    Ok(())
}
