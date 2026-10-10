# uninstall.ps1 - arret et suppression du service kernel ext4fs
#
# ASCII pur volontaire (voir build.ps1 : PowerShell 5.1 + .ps1 sans BOM).
#
# Usage : .\scripts\uninstall.ps1   (console admin)

$ErrorActionPreference = "Continue"

sc.exe stop ext4fs 2>&1 | Out-Null
Start-Sleep -Milliseconds 800
sc.exe delete ext4fs
if ($LASTEXITCODE -eq 0) {
    Write-Host "Service ext4fs supprime."
} else {
    Write-Warning "sc delete a echoue (code $LASTEXITCODE) - service deja absent ?"
}

# Attendre la disparition REELLE du service (code 1060), sinon le nom reste
# reserve et le prochain install.ps1 echouera en 1072. 10 s max.
for ($i = 0; $i -lt 20; $i++) {
    sc.exe query ext4fs 2>&1 | Out-Null
    if ($LASTEXITCODE -eq 1060) { break }
    Start-Sleep -Milliseconds 500
}
sc.exe query ext4fs 2>&1 | Out-Null
if ($LASTEXITCODE -ne 1060) {
    Write-Warning "Le service reste marque pour suppression (1072) : fermez services.msc, sinon il sera purge au prochain reboot."
}

$Dest = Join-Path $env:SystemRoot "System32\drivers\ext4drv.sys"
if (Test-Path $Dest) {
    try {
        Remove-Item $Dest -Force
        Write-Host "Retire : $Dest"
    } catch {
        Write-Warning "Impossible de retirer $Dest (pilote encore charge ?). Il sera supprimable apres reboot."
    }
}

Write-Host "Desinstalle." -ForegroundColor Green
