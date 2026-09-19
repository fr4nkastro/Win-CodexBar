#Requires -Version 5.1
<##
.SYNOPSIS
    Verify SignPath output and emit a final release bundle.

.DESCRIPTION
    SignPath returns the three top-level release artifacts after processing the
    signing bundle. This helper verifies the installer, portable executable,
    and nested CLI executable before computing sidecars and the release
    manifest. It builds the final bundle from SignPath output only; it never
    falls back to or modifies the unsigned build directory.
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$SignedOutputDir,
    [Parameter(Mandatory)][string]$FinalOutputDir,
    [Parameter(Mandatory)][string]$Tag,
    [Parameter(Mandatory)][string]$Sha,
    [string]$ExpectedSignerThumbprint = '',
    [string]$Repository = 'nesszer/Win-CodexBar'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'release-pipeline-common.ps1')

function Assert-ValidAuthenticode {
    param(
        [Parameter(Mandatory)][string]$Path,
        [string]$ExpectedSignerThumbprint = ''
    )

    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne 'Valid') {
        throw "Signed release asset '$Path' has Authenticode status '$($signature.Status)'."
    }
    if (-not $signature.SignerCertificate) {
        throw "Signed release asset '$Path' has no signer certificate."
    }
    if (-not [string]::IsNullOrWhiteSpace($ExpectedSignerThumbprint)) {
        $actualThumbprint = ($signature.SignerCertificate.Thumbprint -replace '\s', '').ToUpperInvariant()
        $expectedThumbprint = ($ExpectedSignerThumbprint -replace '\s', '').ToUpperInvariant()
        if ($actualThumbprint -ne $expectedThumbprint) {
            throw "Signed release asset '$Path' has signer thumbprint '$actualThumbprint', expected '$expectedThumbprint'."
        }
    }
    Write-Host "[ok] Authenticode: $(Split-Path $Path -Leaf) ($($signature.SignerCertificate.Subject))"
}

function Get-RelativeFileName {
    param(
        [Parameter(Mandatory)][string]$Root,
        [Parameter(Mandatory)][System.IO.FileInfo]$File
    )

    $prefix = $Root.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    return $File.FullName.Substring($prefix.Length).Replace([IO.Path]::AltDirectorySeparatorChar, [IO.Path]::DirectorySeparatorChar)
}

function Invoke-ManifestEmission {
    param([Parameter(Mandatory)][string]$AssetsDir)

    $manifestScript = Join-Path $PSScriptRoot 'emit-release-manifest.ps1'
    & powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $manifestScript `
        -AssetsDir $AssetsDir `
        -OutputDir $FinalOutputDir `
        -Tag $Tag `
        -Sha $Sha `
        -Repository $Repository
    if ($LASTEXITCODE -ne 0) {
        throw "emit-release-manifest.ps1 failed with exit code $LASTEXITCODE."
    }
}

if (-not (Test-CanonicalReleaseTag $Tag)) {
    throw "Signed release requires a canonical vX.Y.Z tag; received '$Tag'."
}
if ($Sha -notmatch '^[0-9a-fA-F]{40}$') {
    throw "Signed release requires a full immutable SHA; received '$Sha'."
}
if ((Normalize-GitHubRepository $Repository) -ne 'nesszer/win-codexbar') {
    throw 'Repository must be canonical nesszer/Win-CodexBar.'
}
if (-not (Test-Path -LiteralPath $SignedOutputDir -PathType Container)) {
    throw "Missing SignPath output directory: $SignedOutputDir"
}
if (Test-Path -LiteralPath $FinalOutputDir) {
    throw "Refusing to reuse an existing final output directory: $FinalOutputDir"
}
$signedRoot = (Resolve-Path -LiteralPath $SignedOutputDir).Path

$version = Get-ReleaseVersionFromTag $Tag
$signedNames = @(
    "CodexBar-$version-Setup.exe",
    "CodexBar-$version-portable.exe",
    "CodexBarCLI-v$version-windows-x64.zip"
)
$signedFiles = @(Get-ChildItem -LiteralPath $signedRoot -Recurse -File)
$relativeNames = @($signedFiles | ForEach-Object { Get-RelativeFileName -Root $signedRoot -File $_ } | Sort-Object)
$expectedNames = @($signedNames | Sort-Object)
if (($relativeNames -join '|') -ne ($expectedNames -join '|')) {
    throw "SignPath output must contain exactly the three expected top-level files: $($expectedNames -join ', '). Found: $($relativeNames -join ', ')."
}

$signedPaths = @{}
foreach ($name in $signedNames) {
    $signedPaths[$name] = Join-Path $signedRoot $name
    if (-not (Test-Path -LiteralPath $signedPaths[$name] -PathType Leaf)) {
        throw "SignPath output is missing expected asset: $($signedPaths[$name])"
    }
}

Assert-ValidAuthenticode -Path $signedPaths[$signedNames[0]] -ExpectedSignerThumbprint $ExpectedSignerThumbprint
Assert-ValidAuthenticode -Path $signedPaths[$signedNames[1]] -ExpectedSignerThumbprint $ExpectedSignerThumbprint

$cliExtractRoot = Join-Path ([IO.Path]::GetTempPath()) ('win-codexbar-signed-cli-' + [guid]::NewGuid().ToString('N'))
try {
    Expand-Archive -LiteralPath $signedPaths[$signedNames[2]] -DestinationPath $cliExtractRoot -Force
    $cliMatches = @(Get-ChildItem -LiteralPath $cliExtractRoot -Recurse -File -Filter 'codexbar-cli.exe')
    if ($cliMatches.Count -ne 1) {
        throw "Signed CLI ZIP must contain exactly one codexbar-cli.exe; found $($cliMatches.Count)."
    }
    $cliRoot = (Resolve-Path -LiteralPath $cliExtractRoot).Path
    $cliRelativeName = Get-RelativeFileName -Root $cliRoot -File $cliMatches[0]
    if ($cliRelativeName -ne 'codexbar-cli.exe') {
        throw "Signed CLI ZIP must place codexbar-cli.exe at its root; found '$cliRelativeName'."
    }
    $cliFiles = @(Get-ChildItem -LiteralPath $cliRoot -Recurse -File)
    if ($cliFiles.Count -ne 1) {
        throw "Signed CLI ZIP must contain exactly one file, codexbar-cli.exe; found $($cliFiles.Count)."
    }
    Assert-ValidAuthenticode -Path $cliMatches[0].FullName -ExpectedSignerThumbprint $ExpectedSignerThumbprint
} finally {
    if ([IO.Directory]::Exists($cliExtractRoot)) {
        [IO.Directory]::Delete($cliExtractRoot, $true)
    }
}

$finalAssetsDir = Join-Path ([IO.Path]::GetTempPath()) ('win-codexbar-final-assets-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $finalAssetsDir | Out-Null
try {
    foreach ($name in $signedNames) {
        Copy-Item -LiteralPath $signedPaths[$name] -Destination (Join-Path $finalAssetsDir $name) -Force
        $assetPath = Join-Path $finalAssetsDir $name
        $hash = Get-AssetSha256 $assetPath
        "$hash  $name" | Set-Content -LiteralPath "$assetPath.sha256" -Encoding ascii
    }

    Invoke-ManifestEmission -AssetsDir $finalAssetsDir
} finally {
    if ([IO.Directory]::Exists($finalAssetsDir)) {
        [IO.Directory]::Delete($finalAssetsDir, $true)
    }
}

$finalVersionAssets = @(Get-ExpectedReleaseAssetPaths $FinalOutputDir $version)
foreach ($path in $finalVersionAssets) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Final signed bundle is missing: $path"
    }
}
$finalFiles = @(Get-ChildItem -LiteralPath $FinalOutputDir -File | Select-Object -ExpandProperty Name | Sort-Object)
$expectedFinalFiles = @((Get-RequiredReleaseAssets $version) + 'release-manifest.json' | Sort-Object)
if (($finalFiles -join '|') -ne ($expectedFinalFiles -join '|')) {
    throw "Final signed bundle contains unexpected files: $($finalFiles -join ', ')."
}
Assert-AssetMatchesSidecar (Join-Path $FinalOutputDir "CodexBar-$version-Setup.exe")
Assert-AssetMatchesSidecar (Join-Path $FinalOutputDir "CodexBar-$version-portable.exe")
Assert-AssetMatchesSidecar (Join-Path $FinalOutputDir "CodexBarCLI-v$version-windows-x64.zip")
Write-Host "[ok] signed release bundle finalized: $Repository $Tag ($Sha)"
