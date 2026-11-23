# Run from PowerShell on x86_64 Windows.
# irm https://raw.githubusercontent.com/Calastrophe/sub-mesh/main/install.ps1 | iex
# Set MESH_SERVER_URL before installing.

function Install-Mesh {
    $ErrorActionPreference = 'Stop'
    $repo = if ($env:MESH_REPO) { $env:MESH_REPO } else { 'Calastrophe/sub-mesh' }
    $platform = 'x86_64-windows'
    $placeholder = 'http://<server>:7070'
    $installDir = Join-Path $env:LOCALAPPDATA 'Programs\mesh'
    $configDir = Join-Path $env:APPDATA 'mesh'
    $configFile = Join-Path $configDir 'daemon.toml'
    $files = 'mesh-daemon.exe', 'meshctl.exe', 'mesh-daemon.ps1'
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $taskName = "mesh-daemon-$sid"
    $state = Join-Path $installDir '.install'

    function Update-Path([bool]$uninstall) {
        $marker = Join-Path $state 'path'
        if ($uninstall -and -not (Test-Path -LiteralPath $marker)) { return }
        $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
        try {
            $raw = [string]$key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
            $kind = if ($key.GetValueNames() -contains 'Path') { $key.GetValueKind('Path') } else { 'ExpandString' }
            if ($uninstall) {
                $entries = @($raw.Split(';'))
                $index = [Array]::LastIndexOf($entries, $installDir)
                if ($index -ge 0) {
                    $kept = for ($i = 0; $i -lt $entries.Count; $i++) {
                        if ($i -ne $index) { $entries[$i] }
                    }
                    $key.SetValue('Path', ($kept -join ';'), $kind)
                }
                $env:Path = (($env:Path -split ';' | Where-Object { $_ -ne $installDir }) -join ';')
                Remove-Item -LiteralPath $marker
            } else {
                if ($raw.Split(';') -notcontains $installDir) {
                    $updated = if ($raw) { "$raw;$installDir" } else { $installDir }
                    $key.SetValue('Path', $updated, $kind)
                    New-Item -ItemType File -Path $marker -Force | Out-Null
                }
                if (($env:Path -split ';') -notcontains $installDir) {
                    $env:Path = "$env:Path;$installDir"
                }
            }
        } finally {
            $key.Dispose()
        }
    }

    function Stop-Daemon([bool]$uninstall) {
        if (-not (Test-Path -LiteralPath (Join-Path $state 'task'))) { return }
        $task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue |
            Where-Object { $_.TaskPath -eq '\' }
        if ($task) {
            if ($task.State -eq 'Running') {
                $task | Stop-ScheduledTask
            }
            if ($uninstall) { $task | Unregister-ScheduledTask -Confirm:$false }
        }
    }

    function Wait-Daemon {
        $exePath = Join-Path $installDir 'mesh-daemon.exe'
        foreach ($process in Get-Process -Name mesh-daemon -ErrorAction SilentlyContinue) {
            if ($process.Path -eq $exePath -and -not $process.WaitForExit(5000)) {
                throw "stop the daemon at $exePath before continuing"
            }
        }
    }

    function Save-Download([uri]$url, [string]$path, [bool]$allowHttp = $false) {
        for ($i = 0; $i -lt 10; $i++) {
            if (-not $url.IsAbsoluteUri -or
                ($url.Scheme -ne 'https' -and -not ($allowHttp -and $url.Scheme -eq 'http'))) {
                throw "download requires HTTPS: $url"
            }
            $response = $client.GetAsync($url, [Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
            try {
                if ([int]$response.StatusCode -in 301, 302, 303, 307, 308) {
                    if (-not $response.Headers.Location) { throw "missing redirect location: $url" }
                    $url = [uri]::new($url, $response.Headers.Location)
                    continue
                }
                if ([int]$response.StatusCode -ne 200) { throw "download failed: $url ($($response.StatusCode))" }
                $stream = [IO.File]::Create($path)
                try {
                    [void]$response.Content.CopyToAsync($stream).GetAwaiter().GetResult()
                    $length = $response.Content.Headers.ContentLength
                    if ($null -ne $length -and $stream.Length -ne $length) {
                        throw "incomplete download: $url"
                    }
                } finally {
                    $stream.Dispose()
                }
                return
            } finally {
                $response.Dispose()
            }
        }
        throw "too many redirects: $url"
    }

    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($arch -ne 'AMD64') { throw 'mesh supports x86_64 Windows' }

    if ($env:MESH_UNINSTALL) {
        if (-not (Test-Path -LiteralPath $state -PathType Container)) { return }
        Stop-Daemon $true
        Wait-Daemon
        Update-Path $true
        foreach ($file in $files) {
            if (Test-Path -LiteralPath (Join-Path $state $file)) {
                $target = Join-Path $installDir $file
                if (Test-Path -LiteralPath $target) { Remove-Item -LiteralPath $target }
                Remove-Item -LiteralPath (Join-Path $state $file)
            }
        }
        $marker = Join-Path $state 'task'
        if (Test-Path -LiteralPath $marker) { Remove-Item -LiteralPath $marker }
        if (-not (Get-ChildItem -LiteralPath $state -Force)) { Remove-Item -LiteralPath $state }
        if (-not (Get-ChildItem -LiteralPath $installDir -Force)) { Remove-Item -LiteralPath $installDir }
        Write-Host 'Kept mesh config and data.'
        return
    }

    $serverUrl = $env:MESH_SERVER_URL
    if ($serverUrl -and $serverUrl -cnotmatch '\Ahttp://[^\s\p{Cc}"\\]+\z') {
        throw "invalid MESH_SERVER_URL (use http://HOST:7070)"
    }
    if ($repo -cnotmatch '\A[A-Za-z0-9_-]+/[A-Za-z0-9_.-]+\z') { throw 'invalid MESH_REPO' }

    Add-Type -AssemblyName System.Net.Http
    $protocol = [Net.ServicePointManager]::SecurityProtocol
    if ($PSVersionTable.PSVersion.Major -eq 5) {
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    }

    $work = Join-Path ([IO.Path]::GetTempPath()) ('mesh-' + [Guid]::NewGuid().ToString('N'))
    $client = $null
    try {
        [IO.Directory]::CreateDirectory($work) | Out-Null
        $handler = [Net.Http.HttpClientHandler]::new()
        $handler.AllowAutoRedirect = $false
        $client = [Net.Http.HttpClient]::new($handler)
        $client.DefaultRequestHeaders.UserAgent.ParseAdd('mesh-installer')
        $sums = Join-Path $work 'SHA256SUMS'
        $base = $env:MESH_DOWNLOAD_URL
        $version = $env:MESH_VERSION -creplace '\Av', ''
        $allowHttp = $false

        if ($base) {
            $base = $base.TrimEnd('/')
            $allowHttp = ([uri]$base).Scheme -eq 'http'
            Save-Download "$base/SHA256SUMS" $sums $allowHttp
            if (-not $version) {
                $version = @(foreach ($line in Get-Content -LiteralPath $sums) {
                    if ($line -cmatch '\A[0-9a-fA-F]{64}[ \t]+\*?mesh-([0-9]+\.[0-9]+\.[0-9]+)-x86_64-windows\.zip[ \t]*\z') {
                        $Matches[1]
                    }
                }) -join "`n"
            }
        } elseif (-not $version) {
            $latest = Join-Path $work 'latest.json'
            Save-Download "https://api.github.com/repos/$repo/releases/latest" $latest
            $tag = (Get-Content -LiteralPath $latest -Raw | ConvertFrom-Json).tag_name
            if ($tag -cnotmatch '\Av[0-9]+\.[0-9]+\.[0-9]+\z') { throw 'invalid release tag' }
            $version = $tag.Substring(1)
        }
        if ($version -cnotmatch '\A[0-9]+\.[0-9]+\.[0-9]+\z') { throw 'expected exactly one release version in X.Y.Z format' }
        if (-not $base) {
            $base = "https://github.com/$repo/releases/download/v$version"
            Save-Download "$base/SHA256SUMS" $sums
        }
        $asset = "mesh-$version-$platform.zip"
        $hashes = @(foreach ($line in Get-Content -LiteralPath $sums) {
            $fields = $line.Trim() -split '[ \t]+'
            if ($fields.Count -ge 2 -and ($fields[1] -creplace '\A\*', '') -ceq $asset) {
                if ($fields.Count -ne 2 -or $fields[0] -cnotmatch '\A[0-9a-fA-F]{64}\z') {
                    throw "invalid SHA256SUMS entry for $asset"
                }
                $fields[0]
            }
        })
        if ($hashes.Count -ne 1) { throw "expected exactly one SHA256SUMS entry for $asset" }
        $zip = Join-Path $work $asset
        Save-Download "$base/$asset" $zip $allowHttp
        if ((Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash -ne $hashes[0]) {
            throw "checksum mismatch for $asset"
        }
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $archive = [IO.Compression.ZipFile]::OpenRead($zip)
        try {
            $prefix = "mesh-$version-$platform/"
            $allowed = @($prefix) + @($files | ForEach-Object { $prefix + $_ })
            $seen = @()
            foreach ($entry in $archive.Entries) {
                if ($allowed -cnotcontains $entry.FullName -or $seen -ccontains $entry.FullName) {
                    throw "unexpected archive entry: $($entry.FullName)"
                }
                $seen += $entry.FullName
            }
        } finally {
            $archive.Dispose()
        }
        $unpacked = Join-Path $work 'package'
        Expand-Archive -LiteralPath $zip -DestinationPath $unpacked
        $pkg = Join-Path $unpacked "mesh-$version-$platform"

        foreach ($file in $files) {
            $target = Join-Path $installDir $file
            if (-not (Test-Path -LiteralPath (Join-Path $pkg $file) -PathType Leaf)) {
                throw "missing $file in $asset"
            }
            if ((Test-Path -LiteralPath $target) -and -not (Test-Path -LiteralPath (Join-Path $state $file))) {
                throw "existing file is outside this install: $target"
            }
        }
        Stop-Daemon $false
        Wait-Daemon
        [IO.Directory]::CreateDirectory($state) | Out-Null
        foreach ($file in $files) {
            New-Item -ItemType File -Path (Join-Path $state $file) -Force | Out-Null
            Copy-Item -LiteralPath (Join-Path $pkg $file) -Destination (Join-Path $installDir $file) -Force
        }
    } finally {
        if ($client) { $client.Dispose() }
        [Net.ServicePointManager]::SecurityProtocol = $protocol
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }

    Update-Path $false
    if (-not (Test-Path -LiteralPath $configFile)) {
        [IO.Directory]::CreateDirectory($configDir) | Out-Null
        $url = if ($serverUrl) { $serverUrl } else { $placeholder }
        $stream = [IO.File]::Open($configFile, 'CreateNew', 'Write', 'None')
        try {
            $bytes = [Text.Encoding]::UTF8.GetBytes("server_url = `"$url`"`n")
            $stream.Write($bytes, 0, $bytes.Length)
        } finally {
            $stream.Dispose()
        }
    }
    $configured = -not [IO.File]::ReadAllText($configFile).Contains('"' + $placeholder + '"')

    if ($configured -and -not $env:MESH_NO_SERVICE) {
        $marker = Join-Path $state 'task'
        $taskArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $installDir 'mesh-daemon.ps1'),
            '-Exe', (Join-Path $installDir 'mesh-daemon.exe'), '-TaskName', $taskName)
        if (Test-Path -LiteralPath $marker) { $taskArgs += '-Replace' }
        & powershell.exe @taskArgs -NoStart
        if ($LASTEXITCODE -ne 0) { throw 'could not register the logon task' }
        New-Item -ItemType File -Path $marker -Force | Out-Null
        Start-ScheduledTask -TaskName $taskName -TaskPath '\'
    }

    Write-Host "mesh $version is installed."
    if (-not $configured) {
        Write-Host "Set server_url in $configFile."
        if (-not $env:MESH_NO_SERVICE) {
            Write-Host 'Run the installer again to register the logon task.'
        }
    }
    if ($env:MESH_NO_SERVICE) { Write-Host 'Start mesh-daemon from your shell.' }
}

& {
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $mutex = [Threading.Mutex]::new($false, "Local\mesh-install-$sid")
    $locked = $false
    try {
        try { $locked = $mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { $locked = $true }
        if (-not $locked) { throw 'a mesh install is already running' }
        Install-Mesh
    } finally {
        if ($locked) { $mutex.ReleaseMutex() }
        $mutex.Dispose()
    }
}
