# uninstall.ps1 - arret et suppression du service kernel ext4fs
#
# ASCII pur volontaire (voir build.ps1 : PowerShell 5.1 + .ps1 sans BOM).
#
# Usage : .\scripts\uninstall.ps1   (console admin)

$ErrorActionPreference = "Continue"

sc.exe stop ext4fs | Out-Null
Start-Sleep -Milliseconds 800
sc.exe delete ext4fs
if ($LASTEXITCODE -eq 0) {
    Write-Host "Service ext4fs supprime."
} else {
    Write-Warning "sc delete a echoue (code $LASTEXITCODE) - service deja absent ?"
}

$Dest = Join-Path $env:SystemRoot "System32\drivers\ext4drv.sys"
if (Test-Path $Dest) {
    Remove-Item $Dest -Force
    Write-Host "Retire : $Dest"
}

Write-Host "Desinstalle." -ForegroundColor Green
