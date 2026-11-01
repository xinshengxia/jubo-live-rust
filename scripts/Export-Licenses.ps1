param(
    [string]$ToolchainRoot = $env:LIVE_HUB_RUST_TOOLCHAIN,
    [string]$OutputDirectory,
    [string]$Target = 'x86_64-pc-windows-gnu'
)
$ErrorActionPreference = 'Stop'
$repoPath = Split-Path -Parent $PSScriptRoot
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $repoPath 'licenses/rust' }
$outputPath = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $outputPath -Force | Out-Null
$metadataText = & (Join-Path $PSScriptRoot 'Invoke-Cargo.ps1') -ToolchainRoot $ToolchainRoot metadata --locked --offline --format-version 1 --filter-platform $Target
$metadata = ($metadataText -join "`n") | ConvertFrom-Json
$upstreamRoot = Join-Path $repoPath 'licenses/upstream'
$upstreamManifestPath = Join-Path $upstreamRoot 'SOURCES.json'
$upstreamSnapshots = @()
if (Test-Path -LiteralPath $upstreamManifestPath) {
    $upstreamSnapshots = @((Get-Content -LiteralPath $upstreamManifestPath -Raw | ConvertFrom-Json).components)
}
$nodes = @{}
foreach ($node in $metadata.resolve.nodes) { $nodes[$node.id] = $node }
$used = [Collections.Generic.HashSet[string]]::new()
$queue = [Collections.Generic.Queue[string]]::new()
$queue.Enqueue($metadata.resolve.root)
while ($queue.Count) {
    $id = $queue.Dequeue()
    if ($used.Add($id)) { foreach ($dep in $nodes[$id].dependencies) { $queue.Enqueue($dep) } }
}
$records = @()
$missing = @()
foreach ($package in ($metadata.packages | Sort-Object name,version)) {
    if (-not $used.Contains($package.id) -or $package.id -eq $metadata.resolve.root) { continue }
    $sourcePath = Split-Path -Parent $package.manifest_path
    $packageKey = $package.name + '-' + $package.version
    $licenseFiles = @(Get-ChildItem -LiteralPath $sourcePath -File -Recurse | Where-Object {
        $_.Name -match '(LICENSE|LICENCE|COPYING|NOTICE|UNLICENSE|AUTHORS)' -or
        ($package.name -eq 'epaint_default_fonts' -and $_.Extension -eq '.txt') -or
        ($package.license_file -and $_.FullName -eq $package.license_file)
    })
    $commit = $null
    $vcsPath = Join-Path $sourcePath '.cargo_vcs_info.json'
    if (Test-Path -LiteralPath $vcsPath) { $commit = (Get-Content -LiteralPath $vcsPath -Raw | ConvertFrom-Json).git.sha1 }
    $files = @()
    foreach ($licenseFile in ($licenseFiles | Sort-Object FullName -Unique)) {
        $relative = [IO.Path]::GetRelativePath($sourcePath, $licenseFile.FullName)
        if ($relative.StartsWith('..')) { throw 'License file escaped crate directory' }
        $destination = Join-Path (Join-Path $outputPath $packageKey) $relative
        New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
        Copy-Item -LiteralPath $licenseFile.FullName -Destination $destination -Force
        $files += [ordered]@{ path=($packageKey+'/'+$relative.Replace('\','/')); sourcePath=$relative.Replace('\','/'); sha256=(Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant() }
    }
    # Some published crates omit the repository's root license files. Retain
    # verified snapshots as well as any font/data notices carried by the crate.
    foreach ($snapshot in ($upstreamSnapshots | Where-Object { $package.name -in $_.packages })) {
        foreach ($original in $snapshot.files) {
            $snapshotFile = [IO.Path]::GetFullPath((Join-Path $upstreamRoot $original.path))
            $snapshotRelative = [IO.Path]::GetRelativePath($upstreamRoot, $snapshotFile)
            if ($snapshotRelative.StartsWith('..') -or [IO.Path]::IsPathRooted($snapshotRelative)) { throw 'Upstream license escaped snapshot directory' }
            $digest = (Get-FileHash -LiteralPath $snapshotFile -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($digest -ne $original.sha256) { throw ('Upstream license hash mismatch for ' + $packageKey) }
            $relative = 'upstream/' + $original.path.Replace('\','/')
            $destination = Join-Path (Join-Path $outputPath $packageKey) $relative
            New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
            Copy-Item -LiteralPath $snapshotFile -Destination $destination -Force
            $files += [ordered]@{
                path=($packageKey+'/'+$relative); sourcePath=$original.sourcePath
                upstreamRepository=$snapshot.repository; snapshotRevision=$snapshot.revision
                snapshotNote=$snapshot.note; sha256=$digest
            }
        }
    }
    if (-not $files.Count) { $missing += $packageKey }
    $records += [ordered]@{name=$package.name;version=$package.version;license=$package.license;repository=$package.repository;revision=$commit;files=@($files)}
}
$manifest = [ordered]@{schema='livehub-rust-crate-licenses/1';target=$Target;packages=@($records);missingOriginals=@($missing)}
$manifest | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $outputPath 'SOURCES.json') -Encoding utf8
[pscustomobject]@{Packages=$records.Count;MissingOriginals=$missing.Count;Missing=@($missing)} | ConvertTo-Json
if ($missing.Count) { throw 'Some crates do not carry a detected original license file; review before distribution.' }
