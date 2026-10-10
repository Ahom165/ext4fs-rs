# build.ps1 - compile ext4drv.sys + ext4ctl.exe + ext4cli.exe
#
# IMPORTANT : ce fichier est en ASCII pur (sans accents, sans tiret cadratin).
# Windows PowerShell 5.1 lit les .ps1 sans BOM dans la page de code systeme
# (CP-1252) : les caracteres non-ASCII y sont mal interpretes et cassent le
# parsing (erreur "Jeton inattendu"). Ne pas remettre d'accents ici.
#
# Prerequis :
#   - Visual Studio 2022 + WDK, ou prompt eWDK (pour le pilote ext4drv)
#   - Rust stable >= 1.91 via rustup (https://rustup.rs)
#   - LLVM/libclang accessible dans le PATH (bindgen via wdk-build)
#
# Usage :
#   .\scripts\build.ps1                     # profil driver-release (defaut)
#   .\scripts\build.ps1 -Profile release    # profil release standard

param(
    [ValidateSet("release", "driver-release")]
    [string]$Profile = "driver-release"
)

$ErrorActionPreference = "Stop"
$Root = Split-Path $PSScriptRoot          # racine du repo (scripts/..)
Set-Location $Root

Write-Host "== ext4fs-rs build (profil $Profile) ==" -ForegroundColor Cyan

# 0. Verifications prealables
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo introuvable - installer Rust via rustup (https://rustup.rs) d'abord"
}
cargo +stable --version | Out-Null
if ($LASTEXITCODE -ne 0) { throw "rustup avec toolchain stable requis" }

# 1. Moteur + CLI (multiplateforme, cible hote)
cargo build -p ext4-core --release
if ($LASTEXITCODE -ne 0) { throw "echec build ext4-core" }

# 2. Pilote kernel - necessite un environnement WDK (prompt eWDK ou VS+WDK)
Push-Location crates/ext4drv
cargo build --profile $Profile --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) {
    Pop-Location
    throw "echec build ext4drv - lancer depuis un prompt WDK/eWDK (voir README, section Prerequis)"
}
Pop-Location

# 3. Outil de controle (user-mode, Windows)
Push-Location crates/ext4ctl
cargo build --release --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) {
    Pop-Location
    throw "echec build ext4ctl"
}
Pop-Location

# 4. Rassemblement des artefacts dans dist\
$drvBin = Join-Path $Root "crates\ext4drv\target\x86_64-pc-windows-msvc\$Profile\ext4drv.dll"
$ctlBin = Join-Path $Root "crates\ext4ctl\target\x86_64-pc-windows-msvc\release\ext4ctl.exe"

if (Test-Path $drvBin) {
    $dist = Join-Path $Root "dist"
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    Copy-Item $drvBin (Join-Path $dist "ext4drv.sys") -Force
    if (Test-Path $ctlBin) {
        Copy-Item $ctlBin (Join-Path $dist "ext4ctl.exe") -Force
    }
    Write-Host "== OK : dist\ext4drv.sys + dist\ext4ctl.exe ==" -ForegroundColor Green
    Write-Host "Etape suivante : .\scripts\sign.ps1 puis .\scripts\install.ps1"
} else {
    Write-Warning "ext4drv.dll introuvable - verifier la cible x86_64-pc-windows-msvc"
}
