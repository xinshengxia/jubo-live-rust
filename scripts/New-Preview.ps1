[CmdletBinding()]
param(
    [string]$ToolchainRoot = $env:LIVE_HUB_RUST_TOOLCHAIN,
    [string]$TargetDirectory,
    [string]$OutputDirectory,
    [switch]$SkipVerify
)
$ErrorActionPreference = 'Stop'
$repoPath = Split-Path -Parent $PSScriptRoot
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $repoPath 'artifacts/preview' }
$outputPath = [IO.Path]::GetFullPath($OutputDirectory)
if ((Test-Path -LiteralPath $outputPath) -and @(Get-ChildItem -LiteralPath $outputPath -Force).Count) {
    throw 'Preview output must be a new or empty directory; existing files are retained.'
}
if (-not $TargetDirectory) {
    $TargetDirectory = if ($ToolchainRoot) { Join-Path $ToolchainRoot 'build/live-hub-rust-preview' } else { Join-Path $repoPath 'target' }
}
$targetPath = [IO.Path]::GetFullPath($TargetDirectory)
if ($targetPath -match '[^\x00-\x7F]') {
    throw ("Release preview target directory must be an ASCII-only path: '$targetPath'. " +
        "GNU ld on the x86_64-pc-windows-gnu target cannot open non-ASCII output paths, so a " +
        "default under this repository path cannot work. Pass -TargetDirectory with an ASCII " +
        "path, for example 'D:\build\live-hub-rust-preview'.")
}
$savedFlags = $env:CARGO_ENCODED_RUSTFLAGS
$savedRustFlags = $env:RUSTFLAGS
try {
    # Keep compiler source paths out of a distributable binary. No host/tool
    # inventory, absolute build roots, or private logs are written to manifests.
    $flags = @('-C', 'link-self-contained=yes', ('--remap-path-prefix=' + $repoPath + '=.'), ('--remap-path-prefix=\\?\' + $repoPath + '=.'))
    # Generated GL bindings and import-library symbols can carry output paths.
    $flags += ('--remap-path-prefix=' + $targetPath + '=/build')
    $flags += ('--remap-path-prefix=\\?\' + $targetPath + '=/build')
    if ($ToolchainRoot) {
        $toolPath = [IO.Path]::GetFullPath($ToolchainRoot)
        $flags += ('--remap-path-prefix=' + $toolPath + '=/toolchain')
        $flags += ('--remap-path-prefix=\\?\' + $toolPath + '=/toolchain')
    }
    if ($env:USERPROFILE) { $flags += ('--remap-path-prefix=' + $env:USERPROFILE + '=/user') }
    $env:RUSTFLAGS = $null
    $env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]0x1f
    & (Join-Path $PSScriptRoot 'Build.ps1') -ToolchainRoot $ToolchainRoot -TargetDirectory $targetPath -Release
    & (Join-Path $PSScriptRoot 'Export-Licenses.ps1') -ToolchainRoot $ToolchainRoot
} finally {
    $env:CARGO_ENCODED_RUSTFLAGS = $savedFlags
    $env:RUSTFLAGS = $savedRustFlags
}
$binary = Join-Path $targetPath 'release/live-hub-rust.exe'
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw 'Release binary missing' }
New-Item -ItemType Directory -Path $outputPath -Force | Out-Null
Copy-Item -LiteralPath $binary -Destination (Join-Path $outputPath 'live-hub-rust.exe')
foreach ($name in @('README.md', 'THIRD_PARTY_NOTICES.md', 'LICENSE')) {
    Copy-Item -LiteralPath (Join-Path $repoPath $name) -Destination (Join-Path $outputPath $name)
}
foreach ($name in @('licenses', 'assets/fonts', 'assets/lua', 'assets/emoji-sheet.png', 'assets/emoji-sheet.txt', 'docs')) {
    $destination = Join-Path $outputPath $name
    New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $repoPath $name) -Destination $destination -Recurse
}
Copy-Item -LiteralPath (Join-Path $repoPath 'Cargo.lock') -Destination (Join-Path $outputPath 'Cargo.lock')
$versionLine = (Get-Content -LiteralPath (Join-Path $repoPath 'Cargo.toml') | Where-Object { $_ -match '^version\s*=' } | Select-Object -First 1)
$version = [regex]::Match($versionLine, '"([^"]+)"').Groups[1].Value
& (Join-Path $PSScriptRoot 'Write-PreviewManifest.ps1') -Path $outputPath -Version $version
if (-not $SkipVerify) {
    # An unverified package must not be handed out as a release preview.
    & (Join-Path $PSScriptRoot 'Test-Preview.ps1') -Path $outputPath -ToolchainRoot $ToolchainRoot
}
