# install.ps1 / uninstall.ps1 — installation du pilote ext4drv en service kernel
#
# Le pilote crée son propre device de contrôle \Device\Ext4Fs dans DriverEntry
# (pas de device PnP) : installation via sc create, sans INF.
# Prérequis : test mode actif (bcdedit /set testsigning on), pilote signé (sign.ps1).
#
# Usage : .\scripts\install.ps1   |   .\scripts\uninstall.ps1

param([switch]$Start)

$ErrorActionPreference = "Stop"
$Root = $PSScriptRoot | Split-Path | Split-Path
$Sys  = Join-Path $Root "dist\ext4drv.sys"

if (-not (Test-Path $Sys)) { throw "dist\ext4drv.sys absent — lancer build.ps1 + sign.ps1 d'abord" }

# Le service kernel exige que le binaire vive quelque part de pérenne :
# C:\Windows\System32\drivers\ext4drv.sys
$Dest = Join-Path $env:SystemRoot "System32\drivers\ext4drv.sys"
Copy-Item $Sys $Dest -Force
Write-Host "Copié : $Dest"

sc.exe create ext4fs type= kernel start= demand binPath= "$Dest" DisplayName= "ext4fs-rs ext4 driver"
if ($LASTEXITCODE -ne 0) { throw "sc create a échoué ($LASTEXITCODE)" }

sc.exe description ext4fs "Pilote ext4 (Rust) — FSD natif (Explorateur) + device de controle \\.\Ext4Fs"
if ($Start) {
    sc.exe start ext4fs
    if ($LASTEXITCODE -ne 0) { throw "sc start a échoué ($LASTEXITCODE)" }
    Write-Host "Pilote démarré. FSD actif : branchez un volume ext4 ou lancez dist\ext4ctl.exe fsd-mount \\.\HarddiskVolumeN" -ForegroundColor Green
    Write-Host "Le volume doit apparaître avec une lettre de lecteur dans l'Explorateur (F5)." -ForegroundColor Green
} else {
    Write-Host "Installé. Démarrage : sc start ext4fs" -ForegroundColor Green
}
