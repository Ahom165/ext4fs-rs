# sign.ps1 — signature de test du pilote ext4drv.sys
#
# Crée (une fois) un certificat auto-signé "ext4fs-rs Test", l'installe dans
# Root + TrustedPublisher, signe ext4drv.sys et vérifie la signature.
# Nécessite le "test mode" Windows déjà activé (bcdedit /set testsigning on).
#
# Usage : .\scripts\sign.ps1

$ErrorActionPreference = "Stop"
$Root = $PSScriptRoot | Split-Path | Split-Path
$Sys  = Join-Path $Root "dist\ext4drv.sys"
$Cert = Join-Path $Root "dist\ext4fs-rs-test.pfx"

if (-not (Test-Path $Sys)) { throw "dist\ext4drv.sys absent — lancer build.ps1 d'abord" }

$CertName = "ext4fs-rs Test"

if (-not (Test-Path $Cert)) {
    Write-Host "== Création du certificat de test $CertName ==" -ForegroundColor Cyan
    $cert = New-SelfSignedCertificate `
        -Subject "CN=$CertName" `
        -Type CodeSigningCert `
        -KeyUsage DigitalSignature `
        -KeyExportPolicy Exportable `
        -KeySpec Signature `
        -KeyLength 2048 `
        -HashAlgorithm SHA256 `
        -NotAfter (Get-Date).AddYears(5)
    $pwdPlain = "ext4fs-rs-test"
    $secPwd = ConvertTo-SecureString -String $pwdPlain -Force -AsPlainText
    Export-PfxCertificate -Cert $cert -FilePath $Cert -Password $secPwd | Out-Null
    Write-Host "Certificat exporté : $Cert (mot de passe: $pwdPlain)"
}

Write-Host "== Installation du certificat (LocalMachine Root + TrustedPublisher) =="
$secPwd = ConvertTo-SecureString -String "ext4fs-rs-test" -Force -AsPlainText
Import-PfxCertificate -FilePath $Cert -CertStoreLocation Cert:\LocalMachine\Root -Password $secPwd | Out-Null
Import-PfxCertificate -FilePath $Cert -CertStoreLocation Cert:\LocalMachine\TrustedPublisher -Password $secPwd | Out-Null

Write-Host "== Signature Authenticode de ext4drv.sys =="
$certThumb = (Get-PfxCertificate -FilePath $Cert).Thumbprint
Set-AuthenticodeSignature -FilePath $Sys -Certificate (Get-Item "Cert:\LocalMachine\My\$certThumb") -HashAlgorithm SHA256 | Format-List

Write-Host "== Vérification =="
Get-AuthenticodeSignature $Sys | Format-List Status, StatusMessage, SignerCertificate

Write-Host "Signé. Le test mode (bcdedit /set testsigning on) doit être actif pour le chargement." -ForegroundColor Green
