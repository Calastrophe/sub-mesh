use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use mesh_providers::{read_or_create_secret, read_secret};

fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("secret-{name}"));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn created_once_and_reused() {
    let dir = temp_dir("reuse").join("nested");
    let path = dir.join("secret");
    assert_eq!(
        read_secret(&path).unwrap_err().kind(),
        ErrorKind::NotFound,
        "reading never creates"
    );

    let first = read_or_create_secret(&path).unwrap();
    assert_eq!(first.len(), 43);
    assert!(
        first
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    );
    assert_eq!(read_or_create_secret(&path).unwrap(), first);
    assert_eq!(read_secret(&path).unwrap(), first);

    let other = read_or_create_secret(&dir.join("other")).unwrap();
    assert_ne!(other, first);

    let mut names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["other", "secret"], "no temp files remain");
    let _ = fs::remove_dir_all(dir.parent().unwrap());
}

#[cfg(unix)]
#[test]
fn file_and_directory_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("modes").join("nested");
    let path = dir.join("secret");
    read_or_create_secret(&path).unwrap();
    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&dir), 0o700);

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        read_secret(&path).unwrap_err().kind(),
        ErrorKind::PermissionDenied
    );
    assert!(read_or_create_secret(&path).is_err());
    let _ = fs::remove_dir_all(dir.parent().unwrap());
}

#[cfg(unix)]
#[test]
fn directory_open_to_writers_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("dir-mode");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("secret");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let secret = read_or_create_secret(&path).unwrap();
    assert_eq!(read_or_create_secret(&path).unwrap(), secret);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(read_or_create_secret(&path).is_err());
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn links_are_refused() {
    let dir = temp_dir("link");
    let path = dir.join("secret");
    let target = dir.join("target");
    read_or_create_secret(&target).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(read_secret(&path).is_err());
    assert!(read_or_create_secret(&path).is_err());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_file_that_is_not_a_secret_is_an_error() {
    let dir = temp_dir("invalid");
    let path = dir.join("secret");
    let secret = read_or_create_secret(&path).unwrap();
    let valid = fs::read(&path).unwrap();
    assert_eq!(valid, secret.as_bytes());

    for content in ["", "short", &format!("{secret}x"), &"!".repeat(43)] {
        fs::write(&path, content).unwrap();
        let error = read_secret(&path).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData, "{content:?}");
        assert_eq!(
            read_or_create_secret(&path).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
    }
    let _ = fs::remove_dir_all(&dir);
}
