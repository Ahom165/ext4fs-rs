# uninstall.ps1 — arrêt et désinstallation du service ext4fs

$ErrorActionPreference = "Continue"

sc.exe stop ext4fs | Out-Null
Start-Sleep -Milliseconds 800
sc.exe delete ext4fs
$Dest = Join-Path $env:SystemRoot "System32\drivers\ext4drv.sys"
if (Test-Path $Dest) { Remove-Item $Dest -Force; Write-Host "Retiré : $Dest" }
Write-Host "Désinstallé." -ForegroundColor Green
