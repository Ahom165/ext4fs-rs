# build.ps1 — compile ext4drv.sys + ext4ctl.exe + ext4cli.exe
# Prérequis : Visual Studio 2022 + WDK (ou eWDK prompt), Rust stable >= 1.91,
# LLVM/libclang (bindgen), cargo-make.
#
# Usage : ./build.ps1 [-Profile release|driver-release]

param(
    [ValidateSet("release", "driver-release")]
    [string]$Profile = "driver-release"
)

$ErrorActionPreference = "Stop"
$Root = $PSScriptRoot | Split-Path | Split-Path   # racine du repo (scripts/..)
Set-Location $Root

Write-Host "== ext4fs-rs build (profil $Profile) ==" -ForegroundColor Cyan

# 0. Vérifications
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo introuvable : installer Rust (rustup) d'abord"
}
cargo +stable --version | Out-Null
if ($LASTEXITCODE -ne 0) { throw "rustup stable requis" }

# 1. Cœur + CLI (multiplateforme)
cargo build -p ext4-core --release
if ($LASTEXITCODE -ne 0) { throw "échec build ext4-core" }

# 2. Pilote kernel (nécessite un environnement WDK : eWDK prompt ou VS+WDK)
Push-Location crates/ext4drv
cargo build --profile $Profile --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw "échec build ext4drv — être dans un prompt WDK/eWDK (voir README)" }
Pop-Location

# 3. Outil de contrôle (user-mode)
Push-Location crates/ext4ctl
cargo build --release --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw "échec build ext4ctl" }
Pop-Location

$drvTarget = Join-Path $Root "crates\ext4drv\target\x86_64-pc-windows-msvc\$Profile\ext4drv.dll"
$ctlTarget = Join-Path $Root "crates\ext4ctl\target\x86_64-pc-windows-msvc\release\ext4ctl.exe"

if (Test-Path $drvTarget) {
    $sysPath = Join-Path $Root "dist"
    New-Item -ItemType Directory -Force -Path $sysPath | Out-Null
    Copy-Item $drvTarget (Join-Path $sysPath "ext4drv.sys") -Force
    if (Test-Path $ctlTarget) { Copy-Item $ctlTarget (Join-Path $sysPath "ext4ctl.exe") -Force }
    Write-Host "== OK : dist\ext4drv.sys + dist\ext4ctl.exe ==" -ForegroundColor Green
    Write-Host "Étape suivante : .\scripts\sign.ps1 puis .\scripts\install.ps1"
} else {
    Write-Warning "ext4drv.dll non trouvé — vérifier la cible x86_64-pc-windows-msvc"
}
