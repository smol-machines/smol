<#
.SYNOPSIS
  Install the self-contained `smol` CLI on Windows (x86_64).

.DESCRIPTION
  Downloads the smol-<version>-windows-x86_64.zip release bundle (smol.exe +
  bundled libkrun runtime + guest agent), verifies its checksum, extracts it to
  a prefix, and adds that prefix to the user PATH. No separate smolvm install is
  required — the bundle is self-contained.

  Run:
    irm https://raw.githubusercontent.com/smol-machines/smol/main/scripts/install.ps1 | iex

.NOTES
  Environment overrides:
    SMOL_VERSION                 install a specific tag (default: latest release)
    SMOL_PREFIX                  install location (default: %LOCALAPPDATA%\Programs\smol)
    SMOL_INSECURE_SKIP_CHECKSUM  =1 to install without verifying the checksum (NOT recommended)
#>

$ErrorActionPreference = "Stop"
$Repo = "smol-machines/smol"
$Platform = "windows-x86_64"

function Info($m) { Write-Host ">> $m" }

if ([Environment]::Is64BitOperatingSystem -eq $false) {
    throw "smol requires 64-bit Windows (x86_64)."
}

# ── resolve version ─────────────────────────────────────────────────────────
$Version = $env:SMOL_VERSION
if (-not $Version) {
    Info "resolving latest release of $Repo"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
        -Headers @{ "User-Agent" = "smol-install" }
    $Version = $rel.tag_name
}
$Ver  = $Version.TrimStart("v")
$Dist = "smol-$Ver-$Platform"
$Zip  = "$Dist.zip"
$Base = "https://github.com/$Repo/releases/download/$Version"

$Tmp = Join-Path $env:TEMP ("smol-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $Tmp | Out-Null
try {
    Info "installing smol $Version ($Platform)"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Info "downloading $Zip"
    Invoke-WebRequest -Uri "$Base/$Zip" -OutFile "$Tmp\$Zip" -UseBasicParsing

    # ── verify checksum (combined checksums.sha256; fall back to per-asset) ──
    $want = $null
    try {
        Invoke-WebRequest -Uri "$Base/checksums.sha256" -OutFile "$Tmp\checksums.sha256" -UseBasicParsing
        $line = Get-Content "$Tmp\checksums.sha256" |
            Where-Object { $_ -match [regex]::Escape($Zip) } | Select-Object -First 1
        if ($line) { $want = ($line -split '\s+')[0] }
    } catch {}
    if (-not $want) {
        try {
            Invoke-WebRequest -Uri "$Base/$Zip.sha256" -OutFile "$Tmp\$Zip.sha256" -UseBasicParsing
            $want = ((Get-Content "$Tmp\$Zip.sha256") -split '\s+')[0]
        } catch {}
    }
    if ($want) {
        Info "verifying checksum"
        $got = (Get-FileHash "$Tmp\$Zip" -Algorithm SHA256).Hash.ToLower()
        if ($got -ne $want.ToLower()) { throw "checksum mismatch (want '$want', got '$got')" }
    } elseif ($env:SMOL_INSECURE_SKIP_CHECKSUM -eq "1") {
        Info "WARNING: checksum unavailable; SMOL_INSECURE_SKIP_CHECKSUM=1 — installing unverified"
    } else {
        throw "checksum not found for $Zip (set SMOL_INSECURE_SKIP_CHECKSUM=1 to override)"
    }

    # ── install ─────────────────────────────────────────────────────────────
    $Prefix = $env:SMOL_PREFIX
    if (-not $Prefix) { $Prefix = Join-Path $env:LOCALAPPDATA "Programs\smol" }
    if (Test-Path $Prefix) { Remove-Item -Recurse -Force $Prefix }
    New-Item -ItemType Directory -Force -Path $Prefix | Out-Null

    Info "installing to $Prefix"
    Expand-Archive -Path "$Tmp\$Zip" -DestinationPath $Tmp -Force
    # The zip holds a top-level "$Dist\" folder — move its contents into $Prefix
    # so smol.exe (and its DLLs/templates) land directly in the prefix.
    Copy-Item -Path (Join-Path $Tmp "$Dist\*") -Destination $Prefix -Recurse -Force

    # ── add to the user PATH ────────────────────────────────────────────────
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($userPath -split ';') -notcontains $Prefix) {
        [Environment]::SetEnvironmentVariable("Path", ($userPath.TrimEnd(';') + ";$Prefix"), "User")
        Info "added $Prefix to your PATH (open a new terminal to pick it up)"
    }
    $env:Path = "$env:Path;$Prefix"

    $installed = & "$Prefix\smol.exe" --version
    Info "installed: $installed"
    Write-Host ""
    Write-Host "smol needs the Windows Hypervisor Platform to boot VMs. If it's not enabled:"
    Write-Host "  dism /online /enable-feature /featurename:HypervisorPlatform /all   (admin, then reboot)"
    Write-Host "Then try:  smol run -I alpine --net -- cat /etc/os-release"
}
finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
