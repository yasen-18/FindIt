<#
.SYNOPSIS
    Builds every file a FindIt user downloads.

.DESCRIPTION
    Produces, in .\dist-release:
      FindIt-portable-x64.exe   single exe, no installer needed
      FindIt-portable-x64.zip   the exe plus portable.flag (keeps data next to the exe)
      FindIt-Setup-x64.exe      per-user installer (no administrator rights needed)
      SHA256SUMS.txt            checksums of the three files above

    Code signing is optional. It runs only when one of these is set:
      FINDIT_SIGN_THUMBPRINT     SHA-1 thumbprint of a certificate in the user's store
      FINDIT_SIGN_CERT           path to a .pfx file (with FINDIT_SIGN_PASSWORD)
    FINDIT_SIGN_TIMESTAMP_URL overrides the timestamp server
    (default http://timestamp.digicert.com).

.PARAMETER SkipTests
    Do not run the Rust test suite before building.

.EXAMPLE
    .\build.ps1
#>
[CmdletBinding()]
param(
    [switch]$SkipTests
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Invoke-Checked {
    param([string]$Exe, [string[]]$Arguments)
    Write-Host "  > $Exe $($Arguments -join ' ')" -ForegroundColor DarkGray
    # cargo and npm print progress on stderr. Windows PowerShell 5.1 turns that into
    # errors when output is redirected, so judge success by the exit code only.
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Exe @Arguments 2>&1 | ForEach-Object {
            if ($_ -is [System.Management.Automation.ErrorRecord]) { $_.Exception.Message } else { $_ }
        }
    } finally {
        $ErrorActionPreference = $saved
    }
    if ($LASTEXITCODE -ne 0) {
        throw "'$Exe $($Arguments -join ' ')' failed with exit code $LASTEXITCODE"
    }
}

function Find-SignTool {
    $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    if (Test-Path $kits) {
        $found = Get-ChildItem $kits -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match '\\x64\\' } |
            Sort-Object FullName -Descending |
            Select-Object -First 1
        if ($found) { return $found.FullName }
    }
    return $null
}

$root = $PSScriptRoot
Push-Location $root
try {
    Write-Host '==> Checking tools' -ForegroundColor Cyan
    foreach ($tool in 'cargo', 'npm', 'npx') {
        if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
            throw "$tool was not found. Install the prerequisites listed in README.md (Build from source)."
        }
    }

    $version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "([^"]+)"' |
        Select-Object -First 1).Matches[0].Groups[1].Value
    Write-Host "    FindIt version $version"

    Write-Host '==> Installing JavaScript packages (exact versions from package-lock.json)' -ForegroundColor Cyan
    Invoke-Checked 'npm' @('ci', '--no-audit', '--no-fund')

    if (-not $SkipTests) {
        Write-Host '==> Running tests' -ForegroundColor Cyan
        Invoke-Checked 'npm' @('run', 'build')
        Invoke-Checked 'cargo' @('test', '--workspace', '--release', '--locked')
    }

    # Optional signing: Tauri signs the app exe and the installer with this command.
    $tauriArgs = @('tauri', 'build', '--', '--locked')
    $signArgs = $null
    $configFile = $null
    $timestamp = if ($env:FINDIT_SIGN_TIMESTAMP_URL) { $env:FINDIT_SIGN_TIMESTAMP_URL } else { 'http://timestamp.digicert.com' }
    if ($env:FINDIT_SIGN_THUMBPRINT -or $env:FINDIT_SIGN_CERT) {
        $signtool = Find-SignTool
        if (-not $signtool) { throw 'Signing was requested but signtool.exe was not found. Install the Windows SDK.' }
        if ($env:FINDIT_SIGN_THUMBPRINT) {
            $signArgs = "sign /fd SHA256 /tr $timestamp /td SHA256 /sha1 $($env:FINDIT_SIGN_THUMBPRINT)"
        } else {
            if (-not (Test-Path $env:FINDIT_SIGN_CERT)) { throw "Certificate not found: $($env:FINDIT_SIGN_CERT)" }
            $signArgs = "sign /fd SHA256 /tr $timestamp /td SHA256 /f `"$($env:FINDIT_SIGN_CERT)`""
            if ($env:FINDIT_SIGN_PASSWORD) { $signArgs += " /p `"$($env:FINDIT_SIGN_PASSWORD)`"" }
        }
        $config = @{ bundle = @{ windows = @{ signCommand = "`"$signtool`" $signArgs %1" } } } | ConvertTo-Json -Depth 5 -Compress
        $configFile = Join-Path $env:TEMP "findit-sign-$PID.json"
        Set-Content -Path $configFile -Value $config -Encoding utf8
        $tauriArgs = @('tauri', 'build', '--config', $configFile, '--', '--locked')
        Write-Host '==> Code signing is ON' -ForegroundColor Cyan
    } else {
        Write-Host '==> Code signing is OFF (no certificate configured; Windows SmartScreen will warn on first run)' -ForegroundColor Yellow
    }

    Write-Host '==> Building the release (frontend, Rust, installer)' -ForegroundColor Cyan
    try {
        Invoke-Checked 'npx' $tauriArgs
    } finally {
        if ($signArgs -and $configFile -and (Test-Path $configFile)) { Remove-Item $configFile -Force }
    }

    Write-Host '==> Collecting release files' -ForegroundColor Cyan
    $out = Join-Path $root 'dist-release'
    if (Test-Path $out) { Remove-Item $out -Recurse -Force }
    New-Item -ItemType Directory -Path $out | Out-Null

    $release = Join-Path $root 'target\release'
    $exe = @('FindIt.exe', 'findit.exe') | ForEach-Object { Join-Path $release $_ } | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $exe) { throw "The app executable was not found in $release" }
    $setup = Get-ChildItem (Join-Path $release 'bundle\nsis') -Filter '*-setup.exe' -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $setup) { throw 'The installer was not found in target\release\bundle\nsis' }

    $portable = Join-Path $out 'FindIt-portable-x64.exe'
    Copy-Item $exe $portable
    Copy-Item $setup.FullName (Join-Path $out 'FindIt-Setup-x64.exe')

    $zipDir = Join-Path $env:TEMP "findit-zip-$PID"
    New-Item -ItemType Directory -Path $zipDir -Force | Out-Null
    Copy-Item $exe (Join-Path $zipDir 'FindIt.exe')
    Set-Content -Path (Join-Path $zipDir 'portable.flag') -Value 'FindIt keeps its index and settings in the FindIt-data folder next to FindIt.exe while this file exists.' -Encoding utf8
    Copy-Item (Join-Path $root 'LICENSE') (Join-Path $zipDir 'LICENSE.txt')
    Compress-Archive -Path (Join-Path $zipDir '*') -DestinationPath (Join-Path $out 'FindIt-portable-x64.zip') -Force
    Remove-Item $zipDir -Recurse -Force

    $sums = Get-ChildItem $out -File | Where-Object { $_.Name -ne 'SHA256SUMS.txt' } | Sort-Object Name | ForEach-Object {
        '{0} *{1}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(), $_.Name
    }
    # LF line endings so `sha256sum -c SHA256SUMS.txt` works on any system.
    [System.IO.File]::WriteAllText((Join-Path $out 'SHA256SUMS.txt'), (($sums -join "`n") + "`n"), [System.Text.Encoding]::ASCII)

    Write-Host ''
    Write-Host "Done. FindIt $version release files:" -ForegroundColor Green
    Get-ChildItem $out | ForEach-Object { '  {0,-28} {1,10:N0} KB' -f $_.Name, ($_.Length / 1KB) }
} finally {
    Pop-Location
}
