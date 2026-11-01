[CmdletBinding()]
param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Version)
$ErrorActionPreference = 'Stop'
$packagePath = [IO.Path]::GetFullPath($Path)
if (-not (Test-Path -LiteralPath (Join-Path $packagePath 'live-hub-rust.exe') -PathType Leaf)) { throw 'Preview binary is missing' }
$metrics = @{}
foreach ($file in Get-ChildItem -LiteralPath $packagePath -File -Recurse -Force) {
    $relative = [IO.Path]::GetRelativePath($packagePath, $file.FullName).Replace('\','/')
    if ($relative -in @('manifest.json','manifest.json.sha256')) { continue }
    $metrics[$relative] = @{
        size = $file.Length
        sha256 = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}
# Ordinal path ordering keeps manifests byte-identical across machines, cultures
# and case-insensitive file systems.
$paths = [string[]]@($metrics.Keys)
[Array]::Sort($paths, [StringComparer]::Ordinal)
$entries = @(foreach ($relative in $paths) {
    [ordered]@{path=$relative;size=$metrics[$relative].size;sha256=$metrics[$relative].sha256}
})
$stable = [version](($Version -split '[-+]')[0]) -ge [version]'1.0.0'
$schema = if ($stable) { 'livehub-rust-release/1' } else { 'livehub-rust-preview/1' }
$stage = if ($stable) { 'release' } else { 'migration-preview' }
$sourceState = if ($stable) { 'local-source-build' } else { 'uncommitted-development-preview' }
[ordered]@{schema=$schema;version=$Version;stage=$stage;sourceState=$sourceState;target='windows-x64';files=$entries} |
    ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $packagePath 'manifest.json') -Encoding utf8
$manifestHash = (Get-FileHash -LiteralPath (Join-Path $packagePath 'manifest.json') -Algorithm SHA256).Hash.ToLowerInvariant()
($manifestHash + '  manifest.json') | Set-Content -LiteralPath (Join-Path $packagePath 'manifest.json.sha256') -Encoding ascii
[pscustomobject]@{Stage=$stage;Files=$entries.Count;BinaryBytes=(Get-Item -LiteralPath (Join-Path $packagePath 'live-hub-rust.exe')).Length;ManifestSHA256=$manifestHash} | ConvertTo-Json
