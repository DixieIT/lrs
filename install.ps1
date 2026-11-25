<#
.SYNOPSIS
  Build lrs from this checkout and put the result on PATH.

.DESCRIPTION
  Windows refuses to overwrite a running .exe, so the old one is renamed
  aside and cleared on the next update. You can update from inside a live
  session and get the new binary next time you start it.

.EXAMPLE
  .\install.ps1
  .\install.ps1 -BinDir C:\tools\bin
#>
[CmdletBinding()]
param(
    [string] $BinDir = "$env:USERPROFILE\.local\bin",
    [switch] $Pull
)

$ErrorActionPreference = 'Stop'
$crate = $PSScriptRoot
cd $crate

if ($Pull) {
    Write-Host "==> git pull"
    git pull --ff-only
}

# Through build.ps1, never bare cargo: it puts the right 64-bit mingw-w64 on
# PATH, which the C parts (ring, QuickJS, SQLite) need to compile.
Write-Host "==> build.ps1 build --release"
& (Join-Path $crate 'build.ps1') build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

if (-not (Test-Path $BinDir)) { New-Item -ItemType Directory -Force $BinDir | Out-Null }

$target = Join-Path $BinDir 'lrs.exe'
$stale  = Join-Path $BinDir 'lrs.exe.old'

if (Test-Path $target) {
    if (Test-Path $stale) {
        try { Remove-Item $stale -Force -ErrorAction Stop } catch { Write-Warning "cannot clear old binary" }
    }
    try { Move-Item $target $stale -Force -ErrorAction Stop } catch { Write-Warning "running binary; copying in place" }
}

Copy-Item (Join-Path $crate 'target\release\lrs.exe') $target -Force

if (($env:PATH -split ';') -notcontains $BinDir) {
    Write-Warning "$BinDir is not on PATH  add it so 'lrs' resolves"
}

Write-Host "installed $target"
