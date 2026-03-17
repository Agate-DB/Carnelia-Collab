# install.ps1 — download and install carnelia-collab from GitHub Releases
#
# Usage (run in PowerShell as Administrator or adjust $InstallDir):
#   irm https://raw.githubusercontent.com/Agate-DB/Carnelia-Collab/master/install.ps1 | iex
#   $env:VERSION="v0.1.1"; irm .../install.ps1 | iex

param(
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\carnelia-collab",
    [string]$Version    = $env:VERSION
)

$ErrorActionPreference = "Stop"

$Repo = "Agate-DB/Carnelia-Collab"
$Bin  = "carnelia-collab"

# --- resolve version ---
if (-not $Version) {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest"
    $Version = $release.tag_name
}

if (-not $Version) {
    Write-Error "Could not determine latest release version."
}

$Target  = "x86_64-pc-windows-msvc"
$Archive = "${Bin}-${Target}.zip"
$Url     = "https://github.com/$Repo/releases/download/$Version/$Archive"

Write-Host "Installing $Bin $Version..."
Write-Host "Downloading $Url"

# --- download and extract ---
$TmpDir = Join-Path $env:TEMP "carnelia-install-$(Get-Random)"
New-Item -ItemType Directory -Path $TmpDir | Out-Null

try {
    $ArchivePath = Join-Path $TmpDir $Archive
    Invoke-WebRequest -Uri $Url -OutFile $ArchivePath
    Expand-Archive -Path $ArchivePath -DestinationPath $TmpDir -Force

    # --- install ---
    if (-not (Test-Path $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir | Out-Null
    }
    $Dest = Join-Path $InstallDir "${Bin}.exe"
    Move-Item -Path (Join-Path $TmpDir "${Bin}.exe") -Destination $Dest -Force

    Write-Host ""
    Write-Host "$Bin $Version installed to $Dest"

    # Add to user PATH if missing
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
        Write-Host ""
        Write-Host "Added $InstallDir to your user PATH."
        Write-Host "Restart your terminal for the change to take effect."
    }
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
