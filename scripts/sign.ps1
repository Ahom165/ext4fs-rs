# sign.ps1 - signature de test du pilote ext4drv.sys
#
# ASCII pur volontaire (voir build.ps1 : PowerShell 5.1 + .ps1 sans BOM).
#
# Cree (une seule fois) un certificat de signature de code auto-signe
# "ext4fs-rs Test", l'installe dans les magasins LocalMachine Root et
# TrustedPublisher, signe dist\ext4drv.sys en Authenticode SHA256, puis
# verifie la signature. Le script est idempotent : re-execution sans effet.
#
# Prerequis : console administrateur, test mode deja actif :
#   bcdedit /set testsigning on
#
# Usage : .\scripts\sign.ps1
#
# Note PS 5.1 : Get-PfxCertificate n'accepte PAS -Password (ajoute en PS 6.2)
# et echoue avec "Le mot de passe reseau specifie est incorrect". Ce script
# ne demande JAMAIS de mot de passe : le certificat est reuse depuis le
# magasin de certificats, jamais recharge depuis le PFX.

$ErrorActionPreference = "Stop"
$Root    = Split-Path $PSScriptRoot
$SysPath = Join-Path $Root "dist\ext4drv.sys"
$PfxPath = Join-Path $Root "dist\ext4fs-rs-test.pfx"
$CertName    = "ext4fs-rs Test"
$PfxPassword = "ext4fs-rs-test"

if (-not (Test-Path $SysPath)) {
    throw "dist\ext4drv.sys absent - mettre le dist\ de la release (ou lancer scripts\build.ps1)"
}

$secPwd = ConvertTo-SecureString -String $PfxPassword -Force -AsPlainText

# 1. Certificat de test : reuse s'il existe deja, sinon cree + exporte en PFX
$signCert = Get-ChildItem Cert:\CurrentUser\My, Cert:\LocalMachine\My, `
    Cert:\LocalMachine\TrustedPublisher -ErrorAction SilentlyContinue |
    Where-Object { $_.Subject -eq "CN=$CertName" -and $_.HasPrivateKey } |
    Sort-Object NotAfter -Descending | Select-Object -First 1

if ($signCert) {
    Write-Host "== Certificat existant trouve : $($signCert.Thumbprint) ==" -ForegroundColor Cyan
} else {
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
    $signCert = $codeCert
    Write-Host "Certificat cree : $($signCert.Thumbprint)"
}

# PFX de secours (backup) : re-exporte s'il manque
if (-not (Test-Path $PfxPath)) {
    Export-PfxCertificate -Cert $signCert -FilePath $PfxPath -Password $secPwd | Out-Null
    Write-Host "Certificat exporte : $PfxPath (mot de passe : $PfxPassword)"
}

# 2. Installation dans LocalMachine Root + TrustedPublisher (idempotent)
Write-Host "== Installation du certificat (LocalMachine Root + TrustedPublisher) =="
foreach ($storeName in @("Root", "TrustedPublisher")) {
    $present = Get-ChildItem "Cert:\LocalMachine\$storeName" -ErrorAction SilentlyContinue |
        Where-Object Thumbprint -eq $signCert.Thumbprint
    if (-not $present) {
        Import-PfxCertificate -FilePath $PfxPath `
            -CertStoreLocation "Cert:\LocalMachine\$storeName" -Password $secPwd | Out-Null
        Write-Host "  importe dans LocalMachine\$storeName"
    } else {
        Write-Host "  deja present dans LocalMachine\$storeName"
    }
}

# 3. Signature Authenticode du pilote (certificat du magasin, cle privee incluse)
Write-Host "== Signature Authenticode de ext4drv.sys =="
Set-AuthenticodeSignature -FilePath $SysPath `
    -SigningCert $signCert -HashAlgorithm SHA256 |
    Format-List Status, StatusMessage, SignerCertificate

# 4. Verification
Write-Host "== Verification =="
Get-AuthenticodeSignature -FilePath $SysPath |
    Format-List Status, StatusMessage, SignerCertificate

Write-Host "Pilote signe. Le test mode doit rester actif pour le chargement :" -ForegroundColor Green
Write-Host "  bcdedit /set testsigning on" -ForegroundColor Green
