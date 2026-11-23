use std::fs;
use std::path::PathBuf;

use chrono::{TimeZone, Utc};
use mesh_core::{AccountId, Provider};
use mesh_providers::{Credentials, StoredAccount, TokenStore};

fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("token_store-{name}"));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn account(id: &str, provider: Provider) -> StoredAccount {
    StoredAccount {
        id: id.parse::<AccountId>().unwrap(),
        provider,
        label: Some("me@example.com".to_owned()),
        credentials: Credentials {
            access_token: format!("access-{id}"),
            refresh_token: Some(format!("refresh-{id}")),
            expires_at: Some(Utc.with_ymd_and_hms(2030, 1, 2, 3, 4, 5).unwrap()),
            chatgpt_account_id: (provider == Provider::Codex).then(|| "acct".to_owned()),
            email: Some("me@example.com".to_owned()),
        },
    }
}

#[test]
fn saves_loads_and_removes() {
    let dir = temp_dir("roundtrip");
    let store = TokenStore::new(&dir);
    assert!(
        store.load_all().unwrap().is_empty(),
        "a missing directory is an empty store"
    );

    let codex = account("codex-0a1b2c3d", Provider::Codex);
    let claude = account("claude-ffeeddcc", Provider::Claude);
    store.save(&claude).unwrap();
    store.save(&codex).unwrap();

    let loaded = store.load_all().unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].id, claude.id, "sorted by id");
    assert_eq!(loaded[1].id, codex.id);
    assert_eq!(loaded[1].credentials.access_token, "access-codex-0a1b2c3d");
    assert_eq!(
        loaded[1].credentials.chatgpt_account_id.as_deref(),
        Some("acct")
    );
    assert_eq!(
        loaded[1].credentials.expires_at,
        codex.credentials.expires_at
    );

    let mut updated = codex.clone();
    updated.credentials.access_token = "rotated".to_owned();
    store.save(&updated).unwrap();
    let loaded = store.load_all().unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[1].credentials.access_token, "rotated");

    store.remove(&codex.id).unwrap();
    assert_eq!(store.load_all().unwrap().len(), 1);
    store.remove(&codex.id).unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn files_are_private_and_no_temp_files_remain() {
    let dir = temp_dir("modes").join("nested");
    let store = TokenStore::new(&dir);
    store
        .save(&account("claude-00000001", Provider::Claude))
        .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode =
            |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("claude-00000001.json")), 0o600);
    }

    let names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["claude-00000001.json"]);
    let _ = fs::remove_dir_all(dir.parent().unwrap());
}

#[test]
fn skips_other_files_and_logs_malformed_ones() {
    let dir = temp_dir("skips");
    let store = TokenStore::new(&dir);
    store
        .save(&account("codex-11111111", Provider::Codex))
        .unwrap();

    fs::write(dir.join("notes.txt"), "not an account").unwrap();
    fs::write(dir.join(".codex-22222222.json.tmp"), "{").unwrap();
    fs::write(dir.join("broken.json"), "{ not json").unwrap();
    fs::write(dir.join("empty.json"), "").unwrap();
    fs::create_dir(dir.join("subdir.json")).unwrap();
    let misnamed = serde_json::to_string(&account("codex-33333333", Provider::Codex)).unwrap();
    fs::write(dir.join("codex-44444444.json"), misnamed).unwrap();
    let unsafe_id = serde_json::to_string(&account("codex-55555555", Provider::Codex))
        .unwrap()
        .replace("codex-55555555", "../evil");
    fs::write(dir.join("evil.json"), unsafe_id).unwrap();

    #[cfg(unix)]
    for name in [
        "broken.json",
        "empty.json",
        "codex-44444444.json",
        "evil.json",
    ] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.join(name), fs::Permissions::from_mode(0o600)).unwrap();
    }

    let loaded = store.load_all().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id.as_str(), "codex-11111111");
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn skips_symlinked_accounts() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = temp_dir("symlinks");
    let source = TokenStore::new(root.join("source"));
    let store = TokenStore::new(root.join("destination"));
    source
        .save(&account("codex-linked", Provider::Codex))
        .unwrap();
    fs::create_dir(store.dir()).unwrap();
    fs::set_permissions(store.dir(), fs::Permissions::from_mode(0o700)).unwrap();
    symlink(
        source.dir().join("codex-linked.json"),
        store.dir().join("codex-linked.json"),
    )
    .unwrap();
    assert!(store.load_all().unwrap().is_empty());
    assert!(store.load(&"codex-linked".parse().unwrap()).is_err());

    let link = root.join("linked-store");
    symlink(source.dir(), &link).unwrap();
    assert!(TokenStore::new(link).load_all().is_err());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn rejects_existing_store_or_token_with_public_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("public");
    let store = TokenStore::new(&dir);
    let account = account("codex-public", Provider::Codex);
    store.save(&account).unwrap();
    let file = dir.join("codex-public.json");
    fs::set_permissions(file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.load(&account.id).is_err());
    assert!(store.load_all().unwrap().is_empty());

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(store.load_all().is_err());
    assert!(store.save(&account).is_err());
    fs::remove_dir_all(dir).unwrap();
}
