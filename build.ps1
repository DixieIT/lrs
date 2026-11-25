<#
.SYNOPSIS
  cargo wrapper that puts a 64-bit GNU toolchain ahead of anything stale on PATH.

.DESCRIPTION
  rustc resolves both its linker and `dlltool` by PATH lookup. On a machine where
  an old 32-bit MinGW appears earlier than a modern mingw-w64 — a leftover
  C:\mingw is the usual one — the build fails with either

      dlltool.exe: Can't create .lib file: ... Invalid bfd target
      ld: cannot find -lmsvcrt

  Pinning `linker` in .cargo/config.toml is not enough: it does not affect the
  dlltool lookup. So this script prepends the right directories instead, without
  touching the machine's PATH.

  Everything you pass is forwarded to cargo.

.EXAMPLE
  .\build.ps1 build --release
  .\build.ps1 test
#>
[CmdletBinding()]
param([Parameter(ValueFromRemainingArguments = $true)] [string[]] $CargoArgs)

$ErrorActionPreference = 'Stop'

if (-not $CargoArgs) { $CargoArgs = @('build') }

# Honour an existing rustup install; fall back to the D: layout this repo used.
if (-not $env:RUSTUP_HOME -and (Test-Path 'D:\rust\rustup')) { $env:RUSTUP_HOME = 'D:\rust\rustup' }
if (-not $env:CARGO_HOME  -and (Test-Path 'D:\rust\cargo'))  { $env:CARGO_HOME  = 'D:\rust\cargo' }

# A gcc that actually targets x86_64. Verified by asking it, not by trusting the
# directory name.
$mingwCandidates = @(
    'C:\msys64\ucrt64\bin', 'C:\msys64\mingw64\bin',
    'C:\mingw64\bin', 'C:\tools\mingw64\bin'
)
$mingw = $mingwCandidates | Where-Object {
    $gcc = Join-Path $_ 'gcc.exe'
    (Test-Path $gcc) -and ((& $gcc -dumpmachine 2>$null) -match 'x86_64')
} | Select-Object -First 1

# Order matters, and only one directory goes in front: the mingw one. Adding
# rustup's own `self-contained` binutils here too looks tempting — it holds a
# working dlltool — but it also holds an `ld` that knows nothing about mingw's
# library paths, and putting it first makes every link fail on "cannot find
# -lkernel32". One toolchain, used whole.
$prepend = @()
if ($mingw) {
    $prepend += $mingw
} else {
    Write-Warning "no 64-bit mingw-w64 found in: $($mingwCandidates -join ', ')"
    Write-Warning "install one (e.g. MSYS2 + 'pacman -S mingw-w64-ucrt-x86_64-gcc') if linking fails"
}
if ($env:CARGO_HOME -and (Test-Path "$env:CARGO_HOME\bin")) {
    $prepend += "$env:CARGO_HOME\bin"
}

$env:PATH = (($prepend + ($env:PATH -split ';')) | Where-Object { $_ } | Select-Object -Unique) -join ';'

Write-Host "cargo $($CargoArgs -join ' ')" -ForegroundColor DarkGray
& cargo @CargoArgs
exit $LASTEXITCODE
