use anyhow::{Context, Result, bail};

pub(crate) fn reload(json: bool) -> Result<()> {
    restart_daemon()?;
    if json {
        super::print_json(&serde_json::json!({
            "restarted": true,
            "message": "mesh-daemon restarted and reloaded daemon.toml"
        }))
    } else {
        println!("mesh-daemon restarted and reloaded daemon.toml.");
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn restart_daemon() -> Result<()> {
    use std::process::Command;

    let status = Command::new("systemctl")
        .args(["--user", "restart", "mesh-daemon"])
        .status()
        .context("cannot run systemctl; restart mesh-daemon manually")?;
    if !status.success() {
        bail!("could not restart the mesh-daemon user service; is it installed and running?");
    }
    let active = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "mesh-daemon"])
        .status()
        .context("could not check whether mesh-daemon restarted")?;
    if !active.success() {
        bail!("mesh-daemon did not stay running after restart; check its logs and daemon.toml");
    }
    Ok(())
}

#[cfg(windows)]
fn restart_daemon() -> Result<()> {
    use std::process::Command;

    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$name = "mesh-daemon-$sid"
$task = Get-ScheduledTask -TaskName $name -TaskPath '\' -ErrorAction Stop
if ($task.State -eq 'Running') {
    Stop-ScheduledTask -InputObject $task
    $deadline = (Get-Date).AddSeconds(10)
    do {
        Start-Sleep -Milliseconds 100
        $task = Get-ScheduledTask -TaskName $name -TaskPath '\'
    } while ($task.State -eq 'Running' -and (Get-Date) -lt $deadline)
    if ($task.State -eq 'Running') { throw 'timed out waiting for mesh-daemon to stop' }
}
Start-ScheduledTask -InputObject $task
$deadline = (Get-Date).AddSeconds(10)
do {
    Start-Sleep -Milliseconds 100
    $task = Get-ScheduledTask -TaskName $name -TaskPath '\'
} while ($task.State -ne 'Running' -and (Get-Date) -lt $deadline)
if ($task.State -ne 'Running') { throw 'mesh-daemon did not start; check daemon.toml' }
"#;
    let status = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .status()
        .context("cannot run PowerShell; restart mesh-daemon manually")?;
    if !status.success() {
        bail!("could not restart the mesh-daemon scheduled task; is it installed and running?");
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn restart_daemon() -> Result<()> {
    bail!(
        "automatic daemon restart is supported on Linux and Windows; restart mesh-daemon manually"
    )
}
