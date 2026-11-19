[CmdletBinding()]
param(
    [ValidateSet("client", "host", "both")]
    [string]$Role = "client",

    [switch]$FromSource,

    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA "Programs\Usher"),

    [string]$Repo = "rdenno/usher"
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Split-Path -Parent $ScriptDir
$HasLocalCargo = (Test-Path (Join-Path $RepoRoot "Cargo.toml")) -and (Test-Path (Join-Path $RepoRoot "crates\usher-client"))

$NeedClient = $Role -in @("client", "both")
$NeedHost = $Role -in @("host", "both")

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

function Install-From-Cargo {
    param([string]$Root)
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
    if ($NeedClient) { $pkgs += @("-p", "usher-client") }
    if ($NeedHost) { $pkgs += @("-p", "usher-host") }

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

    if ($NeedClient) {
        Copy-Item (Join-Path $Root "target\release\usher.exe") (Join-Path $InstallDir "usher.exe") -Force
        Write-Host "  [ok] Installed $(Join-Path $InstallDir 'usher.exe')"
    }
    if ($NeedHost) {
        Copy-Item (Join-Path $Root "target\release\usher-host.exe") (Join-Path $InstallDir "usher-host.exe") -Force
        Write-Host "  [ok] Installed $(Join-Path $InstallDir 'usher-host.exe')"
    }
}

$BuildLocally = $FromSource.IsPresent
if (-not $BuildLocally) {
    $LocalClientZip = Join-Path $ScriptDir "usher.exe"
    $LocalHostZip = Join-Path $ScriptDir "usher-host.exe"
    if ((($NeedClient -and (Test-Path $LocalClientZip)) -or -not $NeedClient) -and
        (($NeedHost -and (Test-Path $LocalHostZip)) -or -not $NeedHost)) {
        Write-Host "==> Installing prebuilt binaries from $ScriptDir..."
        if ($NeedClient) {
            Copy-Item $LocalClientZip (Join-Path $InstallDir "usher.exe") -Force
            Write-Host "  [ok] Installed $(Join-Path $InstallDir 'usher.exe')"
        }
        if ($NeedHost) {
            Copy-Item $LocalHostZip (Join-Path $InstallDir "usher-host.exe") -Force
            Write-Host "  [ok] Installed $(Join-Path $InstallDir 'usher-host.exe')"
        }
    } elseif (Get-Command gh -ErrorAction SilentlyContinue) {
        $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("usher-dl-" + [System.Guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
        try {
            Write-Host "==> Downloading latest release from github.com/$Repo..."
            $prevEap = $ErrorActionPreference
            try {
                $ErrorActionPreference = "Continue"
                & gh release download --repo $Repo --pattern "usher-windows-x86_64.zip" --dir $tmpDir
            } finally {
                $ErrorActionPreference = $prevEap
            }
            if ($LASTEXITCODE -eq 0) {
                $zipPath = Join-Path $tmpDir "usher-windows-x86_64.zip"
                $extractDir = Join-Path $tmpDir "extracted"
                Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force
                if ($NeedClient) {
                    Copy-Item (Join-Path $extractDir "usher.exe") (Join-Path $InstallDir "usher.exe") -Force
                    Write-Host "  [ok] Installed $(Join-Path $InstallDir 'usher.exe')"
                }
                if ($NeedHost) {
                    Copy-Item (Join-Path $extractDir "usher-host.exe") (Join-Path $InstallDir "usher-host.exe") -Force
                    Write-Host "  [ok] Installed $(Join-Path $InstallDir 'usher-host.exe')"
                }
            } elseif ($HasLocalCargo) {
                Write-Warning "No published GitHub Release found yet; falling back to building from source."
                $BuildLocally = $true
            } else {
                throw "Failed to download usher-windows-x86_64.zip from $Repo. Check 'gh auth status' or run with -FromSource."
            }
        } finally {
            Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
        }
    } elseif ($HasLocalCargo) {
        Write-Warning "GitHub CLI (gh) not found; building from local repository source."
        $BuildLocally = $true
    } else {
        throw "GitHub CLI (gh) not found and not inside a source checkout. Install 'gh' (https://cli.github.com/) or clone the repo and pass -FromSource."
    }
}

if ($BuildLocally) {
    if (-not $HasLocalCargo) {
        throw "-FromSource requires running from a cloned Usher repository."
    }
    Install-From-Cargo -Root $RepoRoot
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
    Write-Host "==> Added $NormInstall to User PATH (open a new terminal to use it directly)"
} else {
    Write-Host "==> $NormInstall is already on User PATH"
}
if (-not (($env:PATH -split ";") | Where-Object { $_.TrimEnd("\").Equals($NormInstall, [System.StringComparison]::OrdinalIgnoreCase) })) {
    $env:PATH = "$NormInstall;$env:PATH"
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
Write-Host "Done! Open a new terminal and run:"
if ($NeedClient) {
    Write-Host "  usher my-host"
}
if ($NeedHost) {
    Write-Host "  (on client) usher my-pc"
}
