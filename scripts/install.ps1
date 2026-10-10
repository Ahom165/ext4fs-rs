# install.ps1 - installation du pilote ext4drv comme service kernel
#
# ASCII pur volontaire (voir build.ps1 : PowerShell 5.1 + .ps1 sans BOM).
#
# Le pilote cree son device de controle \Device\Ext4Fs (+ lien \DosDevices\Ext4Fs)
# dans DriverEntry, sans device PnP : installation par sc create, sans INF.
# Un service kernel exige un binaire dans System32\drivers : le script y copie
# dist\ext4drv.sys puis cree le service "ext4fs" (demarrage a la demande).
#
# Prerequis : console admin, test mode actif (bcdedit /set testsigning on),
# pilote signe (scripts\sign.ps1).
#
# Usage : .\scripts\install.ps1 [-Start]

param([switch]$Start)

$ErrorActionPreference = "Stop"
$Root    = Split-Path $PSScriptRoot
$SysPath = Join-Path $Root "dist\ext4drv.sys"

if (-not (Test-Path $SysPath)) {
    throw "dist\ext4drv.sys absent - lancer scripts\build.ps1 + scripts\sign.ps1 d'abord"
}

# 1. Copie du pilote dans System32\drivers (emplacement perenne requis
#    par le gestionnaire de services, pas de chemin relatif)
$Dest = Join-Path $env:SystemRoot "System32\drivers\ext4drv.sys"
Copy-Item $SysPath $Dest -Force
Write-Host "Copie : $Dest"

# 2. Creation du service kernel (demarrage a la demande)
sc.exe create ext4fs type= kernel start= demand binPath= "$Dest" DisplayName= "ext4fs-rs ext4 driver"
if ($LASTEXITCODE -ne 0) { throw "sc create a echoue (code $LASTEXITCODE)" }

sc.exe description ext4fs "Pilote ext4 (Rust) - FSD natif pour l'Explorateur + device de controle \\.\Ext4Fs"
if ($LASTEXITCODE -ne 0) { throw "sc description a echoue (code $LASTEXITCODE)" }

if ($Start) {
    sc.exe start ext4fs
    if ($LASTEXITCODE -ne 0) { throw "sc start a echoue (code $LASTEXITCODE)" }
    Write-Host "Pilote demarre. FSD actif : branchez un volume ext4 ou lancez :" -ForegroundColor Green
    Write-Host "  dist\ext4ctl.exe fsd-mount \\.\HarddiskVolumeN" -ForegroundColor Green
    Write-Host "Le volume doit apparaitre avec une lettre dans l'Explorateur (F5)." -ForegroundColor Green
} else {
    Write-Host "Installe. Demarrage manuel : sc start ext4fs" -ForegroundColor Green
}
