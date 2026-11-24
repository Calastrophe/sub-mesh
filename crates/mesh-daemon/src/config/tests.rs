use super::*;

fn own() -> SelfStatus {
    SelfStatus {
        login_name: "alice@example.com".into(),
        host_name: "laptop".into(),
        addresses: vec!["fd7a::1".parse().unwrap(), "100.64.0.5".parse().unwrap()],
    }
}

fn parse(text: &str) -> Config {
    text.parse().expect("config parses")
}

#[test]
fn minimal_config_gets_defaults() {
    let settings = parse(r#"server_url = "http://mesh-server:7070/""#)
        .resolve(Some(&own()), "sysname")
        .unwrap();
    assert_eq!(settings.server_url, "http://mesh-server:7070");
    assert_eq!(settings.node, "laptop");
    assert_eq!(settings.admin_listen, "127.0.0.1:7072".parse().unwrap());
    assert_eq!(settings.poll_interval, Duration::from_secs(90));
    assert_eq!(settings.report_interval, Duration::from_secs(15));
    assert_eq!(settings.reserve, ReserveConfig::default());
    assert_eq!(settings.initial_owner.as_deref(), Some("alice@example.com"));
    assert!(settings.data_dir.ends_with("mesh"));
    assert!(settings.runtime_path().ends_with("mesh/daemon-state.json"));
}

#[test]
fn explicit_values_win() {
    let settings = parse(
        r#"
        server_url = "http://s:1"
        node = "box"
        admin_listen = "[::1]:9001"
        data_dir = "/var/lib/mesh"
        poll_interval_secs = 30
        insecure_skip_whois = true
        [reserve]
        default = 0.5
        [allowed_peers]
        logins = ["bob@example.com"]
        tags = ["tag:mesh"]
        allow_loopback = true
        "#,
    )
    .resolve(None, "sysname")
    .unwrap();
    assert_eq!(settings.node, "box");
    assert_eq!(
        settings.accounts_dir(),
        PathBuf::from("/var/lib/mesh/accounts")
    );
    assert_eq!(settings.poll_interval, Duration::from_secs(30));
    assert_eq!(settings.reserve.default, 0.5);
    assert_eq!(settings.peers.logins, ["bob@example.com"]);
    assert!(settings.peers.skip_whois && settings.peers.allow_loopback);
    assert!(settings.owners.is_empty());
}

#[test]
fn owners_are_configurable() {
    let settings = parse("server_url = \"http://s:1\"\nowners = [\"bob@example.com\"]")
        .resolve(None, "sysname")
        .unwrap();
    assert_eq!(settings.owners, ["bob@example.com"]);
}

#[test]
fn node_falls_back_to_hostname() {
    let settings = parse("server_url = \"http://s:1\"")
        .resolve(None, "sysname")
        .unwrap();
    assert_eq!(settings.node, "sysname");
}

#[test]
fn rejects_bad_config() {
    assert!("".parse::<Config>().is_err(), "server_url is required");
    assert!(
        "server_url = \"http://s\"\nbogus = 1"
            .parse::<Config>()
            .is_err()
    );

    assert!(
        "server_url = \"http://s\"\n[allowed_peers]\nskip_whois = true"
            .parse::<Config>()
            .is_err()
    );

    let non_loopback = parse("server_url = \"http://s:1\"\nadmin_listen = \"100.64.0.5:7072\"");
    assert!(non_loopback.resolve(Some(&own()), "h").is_err());

    let https = parse(r#"server_url = "https://mesh.example.com/""#)
        .resolve(None, "h")
        .expect("https is allowed");
    assert_eq!(https.server_url, "https://mesh.example.com");
    assert_eq!(https.server.port_or_known_default(), Some(443));

    for peers in [
        "logins = [\"a@example.com\"]",
        "tags = [\"tag:mesh\"]",
        "allow_loopback = true",
    ] {
        let config = parse(&format!(
            "server_url = \"https://mesh.example.com\"\n[allowed_peers]\n{peers}"
        ));
        assert!(config.resolve(None, "h").is_err(), "{peers}");
    }

    for url in [
        "ftp://s",
        "ws://s",
        "https://s/path",
        "https://user@s",
        "http://s/path",
        "http://s/?q=1",
        "http://s/#fragment",
        "http://user:secret@s",
    ] {
        let config = parse(&format!("server_url = {url:?}"));
        assert!(config.resolve(Some(&own()), "h").is_err());
    }

    for node in ["\"\"", "\"a\\tb\"", &format!("{:?}", "n".repeat(200))] {
        let config = parse(&format!("server_url = \"http://s:1\"\nnode = {node}"));
        assert!(config.resolve(Some(&own()), "h").is_err(), "{node}");
    }
    let padded = parse("server_url = \"http://s:1\"\nnode = \" box \"");
    assert_eq!(padded.resolve(Some(&own()), "h").unwrap().node, "box");

    let zero = parse("server_url = \"http://s:1\"\npoll_interval_secs = 0");
    assert!(zero.resolve(Some(&own()), "h").is_err());

    let bad_url = parse(r#"server_url = "not a url""#);
    assert!(bad_url.resolve(Some(&own()), "h").is_err());
}

#[test]
fn models_table_sets_defaults_per_provider() {
    let settings = parse(
        r#"
        server_url = "http://s:1"
        [models]
        claude = ["claude-sonnet-*", " Claude-Haiku-* ", "claude-sonnet-*"]
        codex = ["*"]
        "#,
    )
    .resolve(None, "sysname")
    .unwrap();
    assert_eq!(
        settings.models[&Provider::Claude].patterns(),
        ["claude-sonnet-*", "Claude-Haiku-*"]
    );
    assert!(settings.models[&Provider::Codex].allows("anything"));
    let order: Vec<Provider> = settings.models.keys().copied().collect();
    assert_eq!(order, [Provider::Codex, Provider::Claude]);

    let none = parse(r#"server_url = "http://s:1""#)
        .resolve(None, "sysname")
        .unwrap();
    assert!(none.models.is_empty());
}

#[test]
fn rejects_bad_models_table() {
    let parse_err = |models: &str| {
        format!("server_url = \"http://s:1\"\n[models]\n{models}")
            .parse::<Config>()
            .expect_err("config should be rejected")
            .to_string()
    };
    assert!(parse_err("claude = [\"bad pattern\"]").contains("bad pattern"));
    assert!(parse_err("claude = []").contains("empty"));
    assert!(parse_err("claude = [\"\"]").contains("empty"));
    assert!(parse_err("gemini = [\"*\"]").contains("gemini"));
    assert!(parse_err("claude = \"claude-*\"").contains("sequence"));
}
