# Register the daemon for the current Windows user.
# powershell -ExecutionPolicy Bypass -File contrib\mesh-daemon.ps1 -Exe C:\tools\mesh-daemon.exe

param(
    [string]$Exe = (Join-Path $env:LOCALAPPDATA 'Programs\mesh\mesh-daemon.exe'),
    [string]$TaskName,
    [switch]$Replace,
    [switch]$NoStart
)

$ErrorActionPreference = 'Stop'

$exePath = (Get-Item -LiteralPath $Exe).FullName
$user = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $TaskName) { $TaskName = "mesh-daemon-$($user.User.Value)" }

$action = New-ScheduledTaskAction -Execute $exePath
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user.Name
$principal = New-ScheduledTaskPrincipal -UserId $user.Name -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -MultipleInstances IgnoreNew `
    -RestartCount 255 `
    -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName $TaskName -TaskPath '\' -Action $action -Trigger $trigger `
    -Principal $principal -Settings $settings -Force:$Replace | Out-Null
if (-not $NoStart) { Start-ScheduledTask -TaskName $TaskName -TaskPath '\' }
