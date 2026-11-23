use std::process::{Command, Stdio};

use serde_json::{Value, json};

#[test]
fn cancelled_removal_keeps_json_stdout_parseable() {
    let output = Command::new(env!("CARGO_BIN_EXE_meshctl"))
        .args([
            "--admin",
            "http://127.0.0.1:1",
            "--json",
            "remove",
            "claude-1",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("run meshctl");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("Remove account claude-1"), "{stderr}");
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).expect("stdout contains one JSON result"),
        json!({ "cancelled": true })
    );
}

#[tokio::test]
async fn admin_requests_bypass_the_environment_proxy() {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let data_dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("meshctl-proxy");
    let _ = std::fs::remove_dir_all(&data_dir);
    let token = mesh_providers::read_or_create_secret(&data_dir.join("admin-token")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin = format!("http://{}", listener.local_addr().unwrap());
    let server = async {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        let mut line = String::new();
        socket.read_line(&mut line).await.unwrap();
        assert_eq!(line, "GET /admin/v1/status HTTP/1.1\r\n");
        let mut authorized = false;
        while line != "\r\n" {
            line.clear();
            assert!(socket.read_line(&mut line).await.unwrap() > 0);
            if line.to_lowercase().starts_with("authorization:") {
                assert_eq!(
                    line.trim_end().to_lowercase(),
                    format!("authorization: bearer {}", token.to_lowercase())
                );
                authorized = true;
            }
        }
        assert!(authorized, "meshctl sends the admin token");
        let body = r#"{"error":"test admin"}"#;
        let response = format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .get_mut()
            .write_all(response.as_bytes())
            .await
            .unwrap();
    };
    let cli = async {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_meshctl"))
            .args(["--admin", &admin, "--data-dir"])
            .arg(&data_dir)
            .arg("status")
            .env("http_proxy", "http://127.0.0.1:1")
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env_remove("no_proxy")
            .env_remove("NO_PROXY")
            .kill_on_drop(true)
            .output()
            .await
            .unwrap()
    };
    let ((), output) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(server, cli) })
            .await
            .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("test admin"), "{stderr}");
}

#[test]
fn missing_admin_token_is_reported_with_the_data_dir() {
    let data_dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("meshctl-no-token");
    let _ = std::fs::remove_dir_all(&data_dir);
    let output = Command::new(env!("CARGO_BIN_EXE_meshctl"))
        .args(["--admin", "http://127.0.0.1:1", "--data-dir"])
        .arg(&data_dir)
        .arg("status")
        .output()
        .expect("run meshctl");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("is mesh-daemon running as this user?"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("data dir: {}", data_dir.display())),
        "{stderr}"
    );
}
