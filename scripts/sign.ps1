# sign.ps1 - signature de test du pilote ext4drv.sys
#
# ASCII pur volontaire (voir build.ps1 : PowerShell 5.1 + .ps1 sans BOM).
#
# Cree (une seule fois) un certificat de signature de code auto-signe
# "ext4fs-rs Test", l'installe dans les magasins LocalMachine Root et
# TrustedPublisher, signe dist\ext4drv.sys en Authenticode SHA256, puis
# verifie la signature.
#
# Prerequis : console administrateur, test mode deja actif :
#   bcdedit /set testsigning on
#
# Usage : .\scripts\sign.ps1

$ErrorActionPreference = "Stop"
$Root    = Split-Path $PSScriptRoot
$SysPath = Join-Path $Root "dist\ext4drv.sys"
$PfxPath = Join-Path $Root "dist\ext4fs-rs-test.pfx"
$CertName    = "ext4fs-rs Test"
$PfxPassword = "ext4fs-rs-test"

if (-not (Test-Path $SysPath)) {
    throw "dist\ext4drv.sys absent - lancer scripts\build.ps1 d'abord"
}

# 1. Certificat de test : cree une fois, exporte en PFX dans dist\
if (-not (Test-Path $PfxPath)) {
    Write-Host "== Creation du certificat de test '$CertName' ==" -ForegroundColor Cyan
    $codeCert = New-SelfSignedCertificate `
        -Subject "CN=$CertName" `
        -Type CodeSigningCert `
        -KeyUsage DigitalSignature `
        -KeyExportPolicy Exportable `
        -KeySpec Signature `
        -KeyLength 2048 `
        -HashAlgorithm SHA256 `
        -NotAfter (Get-Date).AddYears(5)
    $secPwd = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
    Export-PfxCertificate -Cert $codeCert -FilePath $PfxPath -Password $secPwd | Out-Null
    Write-Host "Certificat exporte : $PfxPath (mot de passe : $PfxPassword)"
}

# 2. Installation dans LocalMachine Root + TrustedPublisher
Write-Host "== Installation du certificat (LocalMachine Root + TrustedPublisher) =="
$secPwd = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText
Import-PfxCertificate -FilePath $PfxPath -CertStoreLocation Cert:\LocalMachine\Root -Password $secPwd | Out-Null
Import-PfxCertificate -FilePath $PfxPath -CertStoreLocation Cert:\LocalMachine\TrustedPublisher -Password $secPwd | Out-Null

# 3. Signature Authenticode du pilote
Write-Host "== Signature Authenticode de ext4drv.sys =="
$thumb = (Get-PfxCertificate -FilePath $PfxPath).Thumbprint
Set-AuthenticodeSignature -FilePath $SysPath `
    -Certificate (Get-Item "Cert:\LocalMachine\My\$thumb") `
    -HashAlgorithm SHA256 | Format-List Status, StatusMessage, SignerCertificate

# 4. Verification
Write-Host "== Verification =="
Get-AuthenticodeSignature -FilePath $SysPath |
    Format-List Status, StatusMessage, SignerCertificate

Write-Host "Pilote signe. Le test mode doit rester actif pour le chargement :" -ForegroundColor Green
Write-Host "  bcdedit /set testsigning on" -ForegroundColor Green
