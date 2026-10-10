# install.ps1 - installation du pilote ext4drv comme service kernel
#
# ASCII pur volontaire (voir build.ps1 : PowerShell 5.1 + .ps1 sans BOM).
#
# Le pilote cree son device de controle \Device\Ext4Fs (+ lien \DosDevices\Ext4Fs)
# dans DriverEntry, sans device PnP : installation par sc create, sans INF.
# Un service kernel exige un binaire dans System32\drivers : le script y copie
# dist\ext4drv.sys puis cree le service "ext4fs" (demarrage a la demande).
#
# Idempotent : si le service existe deja (eventuellement en cours), il est
# arrete puis supprime avant recopie du .sys (le fichier est verrouille par
# le noyau tant que le service tourne - copie impossible sinon).
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
    throw "dist\ext4drv.sys absent - mettre le dist\ de la release (ou lancer scripts\build.ps1 + scripts\sign.ps1)"
}

# 1. Service existant : arret propre puis suppression (idempotent)
sc.exe query ext4fs 2>&1 | Out-Null
if ($LASTEXITCODE -eq 0) {
    Write-Host "Service ext4fs existant : arret puis suppression..."
    sc.exe stop ext4fs 2>&1 | Out-Null
    for ($i = 0; $i -lt 30; $i++) {
        Start-Sleep -Milliseconds 500
        $q = (sc.exe query ext4fs 2>&1) -join " "
        if ($q -match "STOPPED") { break }
    }
    sc.exe delete ext4fs 2>&1 | Out-Null
    Start-Sleep -Seconds 1
}

# 2. Copie du pilote dans System32\drivers (emplacement perenne requis
#    par le gestionnaire de services, pas de chemin relatif)
$Dest = Join-Path $env:SystemRoot "System32\drivers\ext4drv.sys"
Copy-Item $SysPath $Dest -Force
Write-Host "Copie : $Dest"

# 3. Creation du service kernel (demarrage a la demande)
sc.exe create ext4fs type= kernel start= demand binPath= "$Dest" DisplayName= "ext4fs-rs ext4 driver"
if ($LASTEXITCODE -ne 0) { throw "sc create a echoue (code $LASTEXITCODE)" }

sc.exe description ext4fs "Pilote ext4 (Rust) - FSD natif pour l'Explorateur + device de controle \\.\Ext4Fs"
if ($LASTEXITCODE -ne 0) { throw "sc description a echoue (code $LASTEXITCODE)" }

if ($Start) {
    sc.exe start ext4fs
    if ($LASTEXITCODE -ne 0) { throw "sc start a echoue (code $LASTEXITCODE)" }
    Write-Host "Pilote demarre. Verifie l'etat puis teste :" -ForegroundColor Green
    Write-Host "  dist\ext4ctl.exe status          (etat des volumes ext4 montes)" -ForegroundColor Green
    Write-Host "  dist\ext4ctl.exe format \\.\\X: --yes   (EFFACE le volume X !)" -ForegroundColor Green
    Write-Host "  dist\ext4ctl.exe fsd-mount \\.\\X: X:   (lettre dans l'Explorateur)" -ForegroundColor Green
} else {
    Write-Host "Installe. Demarrage manuel : sc start ext4fs" -ForegroundColor Green
}
