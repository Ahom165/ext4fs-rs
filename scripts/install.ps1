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
# Erreur 1072 ("service marque pour suppression") : apres sc delete, le
# gestionnaire de services garde le nom tant qu'un handle reste ouvert
# (services.msc ouvert, antivirus, pilote encore charge...). On attend donc
# la disparition REELLE du service avant de le recreer, sinon sc create
# echoue en 1072.
#
# Les etats sont lus via Get-Service (enum .NET, independant de la langue)
# et les codes de sortie de sc.exe (0 / 1060 / 1072) - jamais via le texte
# de sc query, traduit par Windows ("STOPPED" n'existe pas en francais :
# l'ancienne boucle n'attendait donc jamais et supprimait trop tot).
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

# 0. Preconditions : console admin ; services.msc ouvert = cause n.1 du 1072
$IsAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $IsAdmin) {
    throw "Console administrateur requise (clic droit sur PowerShell -> Executer en tant qu'administrateur)."
}

$Mmc = Get-CimInstance Win32_Process -Filter "Name='mmc.exe'" -ErrorAction SilentlyContinue
if ($Mmc | Where-Object { $_.CommandLine -like "*services.msc*" }) {
    Write-Host "ATTENTION : services.msc est ouvert - il peut bloquer la suppression du" -ForegroundColor Yellow
    Write-Host "service (erreur 1072 au sc create). Fermez-le si l'installation echoue." -ForegroundColor Yellow
}

# Code de sortie de sc query pour ext4fs (independant de la langue) :
#   0 = existe | 1060 = absent | 1072 = marque pour suppression
function Get-ScQueryCode {
    sc.exe query ext4fs 2>&1 | Out-Null
    return $LASTEXITCODE
}

$Code = Get-ScQueryCode

# 1. Service existant : arret propre puis suppression (idempotent)
if ($Code -eq 0) {
    Write-Host "Service ext4fs existant : arret puis suppression..."
    $Svc = Get-Service -Name ext4fs -ErrorAction SilentlyContinue
    if ($Svc -and $Svc.Status -ne "Stopped") {
        sc.exe stop ext4fs 2>&1 | Out-Null
        for ($i = 0; $i -lt 30; $i++) {
            Start-Sleep -Milliseconds 500
            $Svc.Refresh()
            if ($Svc.Status -eq "Stopped") { break }
        }
        if ($Svc.Status -ne "Stopped") {
            Write-Warning ("Arret du service non confirme (etat " + $Svc.Status + ") - " +
                "pilote encore charge ? Tentative de suppression quand meme ; " +
                "si l'installation echoue, redemarrez le PC.")
        }
    }
    sc.exe delete ext4fs 2>&1 | Out-Null
} elseif ($Code -eq 1072) {
    Write-Host "Service ext4fs deja marque pour suppression : attente de la purge..."
}

# 1bis. Attente de la disparition REELLE du service : le SCM retient le nom
#       tant qu'un handle reste ouvert (services.msc, pilote charge, etc.).
#       30 s max, sinon diagnostic complet et sortie propre.
$Gone = $false
for ($i = 0; $i -lt 60; $i++) {
    if ((Get-ScQueryCode) -eq 1060) { $Gone = $true; break }
    Start-Sleep -Milliseconds 500
}
if (-not $Gone) {
    Write-Host ""
    Write-Host "ERREUR 1072 : le service ext4fs reste marque pour suppression." -ForegroundColor Red
    Write-Host "Un programme detient encore un handle dessus. Dans l'ordre :"
    Write-Host "  1. Fermez COMPLETEMENT services.msc (et ses fenetres de proprietes),"
    Write-Host "     attendez ~15 s, relancez install.ps1."
    Write-Host "  2. Sinon redemarrez le PC (purge definitive), puis relancez install.ps1."
    Write-Host "Note : inutile de re-signer, le .sys signe est deja dans System32\drivers."
    exit 1
}

# 2. Copie du pilote dans System32\drivers (emplacement perenne requis
#    par le gestionnaire de services, pas de chemin relatif)
$Dest = Join-Path $env:SystemRoot "System32\drivers\ext4drv.sys"
try {
    Copy-Item $SysPath $Dest -Force
} catch {
    Write-Host ""
    Write-Host "Copie impossible : le pilote est encore charge en memoire (fichier verrouille)." -ForegroundColor Red
    Write-Host "-> Redemarrez le PC puis relancez install.ps1 (inutile de re-signer)."
    exit 1
}
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
