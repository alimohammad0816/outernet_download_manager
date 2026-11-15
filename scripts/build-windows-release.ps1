$ErrorActionPreference = 'Stop'
$config = @{ bundle = @{} }
$certificate = $null
$certificatePath = Join-Path $env:RUNNER_TEMP 'outernet-signing.pfx'
$configPath = Join-Path $env:RUNNER_TEMP 'outernet-release.json'

try {
    if ($env:WINDOWS_CERTIFICATE -or $env:WINDOWS_CERTIFICATE_PASSWORD) {
        if (!$env:WINDOWS_CERTIFICATE -or !$env:WINDOWS_CERTIFICATE_PASSWORD) {
            throw 'Configure both Windows certificate secrets.'
        }
        [IO.File]::WriteAllBytes($certificatePath, [Convert]::FromBase64String($env:WINDOWS_CERTIFICATE))
        $password = ConvertTo-SecureString $env:WINDOWS_CERTIFICATE_PASSWORD -AsPlainText -Force
        $certificates = @(Import-PfxCertificate -FilePath $certificatePath -CertStoreLocation Cert:\CurrentUser\My -Password $password)
        $certificate = $certificates | Where-Object HasPrivateKey | Select-Object -First 1
        if (!$certificate) { throw 'The Windows certificate has no private key.' }
        $config.bundle.windows = @{
            certificateThumbprint = $certificate.Thumbprint
            digestAlgorithm = 'sha256'
            timestampUrl = 'http://timestamp.digicert.com'
            tsp = $true
        }
    }

    if ($env:OUTERNET_UPDATER_PUBLIC_KEY -or $env:TAURI_SIGNING_PRIVATE_KEY -or $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD) {
        if (!$env:OUTERNET_UPDATER_PUBLIC_KEY -or !$env:TAURI_SIGNING_PRIVATE_KEY) {
            throw 'Configure both the updater public key and signing private key.'
        }
        $config.bundle.createUpdaterArtifacts = $true
        $config.plugins = @{ updater = @{
            pubkey = $env:OUTERNET_UPDATER_PUBLIC_KEY.Trim()
            endpoints = @("https://github.com/$env:GITHUB_REPOSITORY/releases/latest/download/latest.json")
        } }
    }

    # Child processes need the Tauri signing key but never need the PFX secrets.
    Remove-Item Env:WINDOWS_CERTIFICATE, Env:WINDOWS_CERTIFICATE_PASSWORD -ErrorAction SilentlyContinue
    Remove-Item $certificatePath -ErrorAction SilentlyContinue
    $config | ConvertTo-Json -Depth 8 | Set-Content $configPath -Encoding utf8
    npm run tauri -- build --bundles nsis --config $configPath -- --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    if ($certificate) {
        $binary = Get-Item 'src-tauri/target/release/outernet.exe'
        $installer = Get-Item 'src-tauri/target/release/bundle/nsis/*-setup.exe'
        foreach ($file in @($binary, $installer)) {
            $signature = Get-AuthenticodeSignature $file.FullName
            if ($signature.Status -ne 'Valid' -or !$signature.TimeStamperCertificate -or
                $signature.SignerCertificate.Thumbprint -ne $certificate.Thumbprint) {
                throw "Invalid or untimestamped Authenticode signature for $($file.Name): $($signature.Status)"
            }
        }
    }
} finally {
    if ($certificate) {
        Remove-Item "Cert:\CurrentUser\My\$($certificate.Thumbprint)" -DeleteKey
    }
    Remove-Item $certificatePath, $configPath -ErrorAction SilentlyContinue
}
