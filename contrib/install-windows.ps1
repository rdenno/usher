[CmdletBinding()]
param(
    [ValidateSet("client", "host", "both")]
    [string]$Role = "client",

    [string]$Version = "latest",

    [switch]$FromSource,

    [switch]$PauseOnExit,

    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "Programs\Usher"),

    [string]$Repo = "rdenno/usher"
)

if (-not $PSBoundParameters.ContainsKey("Role") -and $env:USHER_ROLE) {
    if ($env:USHER_ROLE -in @("client", "host", "both")) {
        $Role = $env:USHER_ROLE
    } else {
        throw "Invalid USHER_ROLE '$($env:USHER_ROLE)'. Expected: client, host, or both."
    }
}
if (-not $PSBoundParameters.ContainsKey("Version") -and $env:USHER_VERSION) {
    $Version = $env:USHER_VERSION
}
if (-not $PSBoundParameters.ContainsKey("InstallDir") -and $env:USHER_INSTALL_DIR) {
    $InstallDir = $env:USHER_INSTALL_DIR
}

$ErrorActionPreference = "Stop"
$ExitCode = 0

function Install-UsherExe {
    param(
        [string]$SourcePath,
        [string]$BinaryName,
        [string]$DestDir
    )
    if (-not (Test-Path $SourcePath)) {
        throw "Expected binary not found: $SourcePath"
    }
    $dest = Join-Path $DestDir $BinaryName
    Copy-Item $SourcePath $dest -Force
    Unblock-File -Path $dest -ErrorAction SilentlyContinue
    Write-Host "  [ok] Installed $dest"
}

function Install-From-Cargo {
    param(
        [string]$Root,
        [string]$DestDir,
        [bool]$InstallClient,
        [bool]$InstallHost
    )
    if (-not $env:CARGO_HOME) {
        $ScoopCargoHome = Join-Path $env:USERPROFILE "scoop\persist\rustup-msvc\.cargo"
        if (Test-Path $ScoopCargoHome) {
            $env:CARGO_HOME = $ScoopCargoHome
        }
    }
    if (-not $env:RUSTUP_HOME) {
        $ScoopRustupHome = Join-Path $env:USERPROFILE "scoop\persist\rustup-msvc\.rustup"
        if (Test-Path $ScoopRustupHome) {
            $env:RUSTUP_HOME = $ScoopRustupHome
        }
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        $ScoopCargoBin = Join-Path $env:USERPROFILE "scoop\apps\rustup-msvc\current\.cargo\bin"
        $DefaultCargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
        foreach ($dir in @($ScoopCargoBin, $DefaultCargoBin)) {
            if (Test-Path (Join-Path $dir "cargo.exe")) {
                $env:PATH = "$dir;$env:PATH"
                break
            }
        }
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "cargo.exe not found on PATH. Install Rust MSVC first or omit -FromSource to download a prebuilt GitHub Release."
    }

    $pkgs = @()
    if ($InstallClient) { $pkgs += @("-p", "usher-client") }
    if ($InstallHost) { $pkgs += @("-p", "usher-host") }

    Write-Host "==> Building release binaries from source in $Root..."
    $prevEap = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        & cargo build --manifest-path (Join-Path $Root "Cargo.toml") --release @pkgs
    } finally {
        $ErrorActionPreference = $prevEap
    }
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release failed with exit code $LASTEXITCODE"
    }

    if ($InstallClient) {
        Install-UsherExe -SourcePath (Join-Path $Root "target\release\usher.exe") -BinaryName "usher.exe" -DestDir $DestDir
    }
    if ($InstallHost) {
        Install-UsherExe -SourcePath (Join-Path $Root "target\release\usher-host.exe") -BinaryName "usher-host.exe" -DestDir $DestDir
    }
}

try {
    $ScriptPath = $MyInvocation.MyCommand.Path
    $ScriptDir = if ($ScriptPath) { Split-Path -Parent $ScriptPath } else { "" }
    $RepoRoot = if ($ScriptDir) {
        Split-Path -Parent $ScriptDir
    } elseif (Test-Path (Join-Path (Get-Location).Path "Cargo.toml")) {
        (Get-Location).Path
    } else {
        ""
    }
    $HasLocalCargo = [bool]$RepoRoot -and (Test-Path (Join-Path $RepoRoot "Cargo.toml")) -and (Test-Path (Join-Path $RepoRoot "crates\usher-client"))

    $NeedClient = $Role -in @("client", "both")
    $NeedHost = $Role -in @("host", "both")

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

    $BuildLocally = $FromSource.IsPresent
    if (-not $BuildLocally) {
        $HasSiblingPrebuilt = $false
        if ($ScriptDir) {
            $LocalClientZip = Join-Path $ScriptDir "usher.exe"
            $LocalHostZip = Join-Path $ScriptDir "usher-host.exe"
            $HasSiblingPrebuilt = (($NeedClient -and (Test-Path $LocalClientZip)) -or -not $NeedClient) -and
                (($NeedHost -and (Test-Path $LocalHostZip)) -or -not $NeedHost)
        }

        if ($HasSiblingPrebuilt) {
            Write-Host "==> Installing prebuilt binaries from $ScriptDir..."
            if ($NeedClient) {
                Install-UsherExe -SourcePath $LocalClientZip -BinaryName "usher.exe" -DestDir $InstallDir
            }
            if ($NeedHost) {
                Install-UsherExe -SourcePath $LocalHostZip -BinaryName "usher-host.exe" -DestDir $InstallDir
            }
        } else {
            $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("usher-dl-" + [System.Guid]::NewGuid().ToString("N"))
            New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
            try {
                $zipPath = Join-Path $tmpDir "usher-windows-x86_64.zip"
                $downloaded = $false
                $zipUrl = if ($Version -eq "latest") {
                    "https://github.com/$Repo/releases/latest/download/usher-windows-x86_64.zip"
                } else {
                    "https://github.com/$Repo/releases/download/$Version/usher-windows-x86_64.zip"
                }

                Write-Host "==> Downloading Usher ($Version) from $zipUrl..."
                $prevProgress = $ProgressPreference
                try {
                    $ProgressPreference = "SilentlyContinue"
                    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
                    Invoke-WebRequest -Uri $zipUrl -OutFile $zipPath -UseBasicParsing
                    if ((Test-Path $zipPath) -and ((Get-Item $zipPath).Length -gt 0)) {
                        $downloaded = $true
                    }
                } catch {
                    Write-Verbose "Direct GitHub download failed: $($_.Exception.Message)"
                } finally {
                    $ProgressPreference = $prevProgress
                }

                if (-not $downloaded -and (Get-Command gh -ErrorAction SilentlyContinue)) {
                    Write-Host "==> Falling back to 'gh release download' for github.com/$Repo..."
                    $prevEap = $ErrorActionPreference
                    try {
                        $ErrorActionPreference = "Continue"
                        $ghArgs = @("release", "download")
                        if ($Version -ne "latest") {
                            $ghArgs += $Version
                        }
                        $ghArgs += @("--repo", $Repo, "--pattern", "usher-windows-x86_64.zip", "--dir", $tmpDir)
                        & gh @ghArgs
                    } finally {
                        $ErrorActionPreference = $prevEap
                    }
                    if ($LASTEXITCODE -eq 0 -and (Test-Path $zipPath)) {
                        $downloaded = $true
                    }
                }

                if ($downloaded) {
                    $extractDir = Join-Path $tmpDir "extracted"
                    Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force
                    if ($NeedClient) {
                        Install-UsherExe -SourcePath (Join-Path $extractDir "usher.exe") -BinaryName "usher.exe" -DestDir $InstallDir
                    }
                    if ($NeedHost) {
                        Install-UsherExe -SourcePath (Join-Path $extractDir "usher-host.exe") -BinaryName "usher-host.exe" -DestDir $InstallDir
                    }
                } elseif ($HasLocalCargo) {
                    Write-Warning "Could not download prebuilt release ($Version); falling back to building from source."
                    $BuildLocally = $true
                } else {
                    throw "Failed to download usher-windows-x86_64.zip ($Version) from $Repo. Check internet access or download usher-setup-windows-x86_64.exe from https://github.com/$Repo/releases."
                }
            } finally {
                Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
            }
        }
    }

    if ($BuildLocally) {
        if (-not $HasLocalCargo) {
            throw "-FromSource requires running from a cloned Usher repository."
        }
        Install-From-Cargo -Root $RepoRoot -DestDir $InstallDir -InstallClient $NeedClient -InstallHost $NeedHost
    }

    # Add InstallDir to User PATH cleanly
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $Parts = @()
    if ($UserPath) {
        $Parts = $UserPath -split ";" | Where-Object { $_ -ne "" }
    }
    $NormInstall = $InstallDir.TrimEnd("\")
    $AlreadyInPath = $Parts | Where-Object { $_.TrimEnd("\").Equals($NormInstall, [System.StringComparison]::OrdinalIgnoreCase) }
    if (-not $AlreadyInPath) {
        $NewPath = (@($Parts) + $NormInstall) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        Write-Host "==> Added $NormInstall to User PATH"
    } else {
        Write-Host "==> $NormInstall is already on User PATH"
    }
    if (-not (($env:PATH -split ";") | Where-Object { $_.TrimEnd("\").Equals($NormInstall, [System.StringComparison]::OrdinalIgnoreCase) })) {
        $env:PATH = "$NormInstall;$env:PATH"
    }

    # Verify installed binaries run cleanly
    Write-Host "==> Verifying installed binaries..."
    if ($NeedClient) {
        $ClientExe = Join-Path $InstallDir "usher.exe"
        if (-not (Test-Path $ClientExe)) {
            throw "Post-install verification failed: missing $ClientExe"
        }
        $prevEap = $ErrorActionPreference
        try {
            $ErrorActionPreference = "Continue"
            & $ClientExe --help | Out-Null
        } finally {
            $ErrorActionPreference = $prevEap
        }
        if ($LASTEXITCODE -ne 0) {
            throw "usher.exe --help exited with code $LASTEXITCODE"
        }
        Write-Host "  [ok] Verified usher.exe runs cleanly"
    }
    if ($NeedHost) {
        $HostExe = Join-Path $InstallDir "usher-host.exe"
        if (-not (Test-Path $HostExe)) {
            throw "Post-install verification failed: missing $HostExe"
        }
        $prevEap = $ErrorActionPreference
        try {
            $ErrorActionPreference = "Continue"
            & $HostExe --print-udev-rule | Out-Null
        } finally {
            $ErrorActionPreference = $prevEap
        }
        if ($LASTEXITCODE -ne 0) {
            throw "usher-host.exe --print-udev-rule exited with code $LASTEXITCODE"
        }
        Write-Host "  [ok] Verified usher-host.exe runs cleanly"
    }

    if ($NeedHost) {
        Write-Host ""
        Write-Host "==> Checking Windows OpenSSH Server (sshd)..."
        $sshd = Get-Service -Name sshd -ErrorAction SilentlyContinue
        if (-not $sshd) {
            Write-Warning "OpenSSH Server (sshd) is not installed. Run in an elevated PowerShell:"
            Write-Host "  Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0"
            Write-Host "  Start-Service sshd; Set-Service -Name sshd -StartupType Automatic"
        } elseif ($sshd.Status -ne "Running") {
            Write-Warning "OpenSSH Server (sshd) is installed but not running (Status: $($sshd.Status)). Start it with:"
            Write-Host "  Start-Service sshd; Set-Service -Name sshd -StartupType Automatic"
        } else {
            Write-Host "  [ok] sshd service is Running"
        }

        $HostExe = Join-Path $InstallDir "usher-host.exe"
        $IsAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
        $RuleName = "Usher Host (UDP)"
        if ($IsAdmin) {
            $Existing = Get-NetFirewallRule -DisplayName $RuleName -ErrorAction SilentlyContinue
            if ($Existing) {
                Remove-NetFirewallRule -DisplayName $RuleName | Out-Null
            }
            New-NetFirewallRule -DisplayName $RuleName -Direction Inbound -Program $HostExe -Protocol UDP -Action Allow -Profile Any | Out-Null
            Write-Host "  [ok] Configured Windows Firewall inbound UDP rule for $HostExe"
        } else {
            Write-Host ""
            Write-Host "==> Optional: allow inbound UDP through Windows Firewall (run in elevated PowerShell if needed):"
            Write-Host "  New-NetFirewallRule -DisplayName `"$RuleName`" -Direction Inbound -Program `"$HostExe`" -Protocol UDP -Action Allow -Profile Any"
        }

        Write-Host ""
        Write-Host "==> Detected host IPv4 addresses (paste one into your laptop's ~/.ssh/config):"
        $ips = Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
            Where-Object { $_.IPAddress -ne "127.0.0.1" -and $_.IPAddress -notlike "169.254.*" } |
            Select-Object InterfaceAlias, IPAddress
        foreach ($ip in $ips) {
            Write-Host ("  - {0,-32} {1}" -f $ip.InterfaceAlias, $ip.IPAddress)
        }
        $PreferredIp = ($ips | Where-Object { $_.InterfaceAlias -match "Tailscale|ZeroTier|WireGuard" } | Select-Object -First 1).IPAddress
        if (-not $PreferredIp) {
            $PreferredIp = ($ips | Select-Object -First 1).IPAddress
        }
        if (-not $PreferredIp) { $PreferredIp = "<host-overlay-ip>" }

        Write-Host ""
        Write-Host "Example ~/.ssh/config entry on your client laptop:"
        Write-Host "  Host my-pc"
        Write-Host "      HostName $PreferredIp"
        Write-Host "      User $env:USERNAME"
        Write-Host "      IdentityFile ~/.ssh/id_ed25519"
    }

    Write-Host ""
    Write-Host "============================================================" -ForegroundColor Green
    Write-Host " Usher installation complete!" -ForegroundColor Green
    Write-Host "============================================================" -ForegroundColor Green
    if ($NeedClient) {
        Write-Host "  Client: $(Join-Path $InstallDir 'usher.exe')"
    }
    if ($NeedHost) {
        Write-Host "  Host:   $(Join-Path $InstallDir 'usher-host.exe')"
    }
    Write-Host ""
    Write-Host "To run 'usher' in a NEW PowerShell window:"
    if ($NeedClient) {
        Write-Host "  usher my-host"
    }
    if ($NeedHost) {
        Write-Host "  (on client) usher my-pc"
    }
    Write-Host ""
    Write-Host "To use 'usher' in THIS already-open PowerShell window without restarting it, run:"
    Write-Host '  $env:PATH = [Environment]::GetEnvironmentVariable("Path","User") + ";" + [Environment]::GetEnvironmentVariable("Path","Machine")'
} catch {
    Write-Host ""
    Write-Host "[ERROR] $($_.Exception.Message)" -ForegroundColor Red
    $ExitCode = 1
} finally {
    if ($PauseOnExit) {
        Write-Host ""
        Read-Host "Press Enter to close" | Out-Null
    }
}

if ($MyInvocation.MyCommand.Path) {
    exit $ExitCode
} elseif ($ExitCode -ne 0) {
    throw "Usher installation failed (exit code $ExitCode)."
}
