[CmdletBinding()]
param([Parameter(Mandatory)][string]$Path, [string]$ToolchainRoot = $env:LIVE_HUB_RUST_TOOLCHAIN, [string[]]$AdditionalPrivatePrefix = @())
$ErrorActionPreference = 'Stop'
$packagePath = [IO.Path]::GetFullPath($Path)
if ((Get-Item -LiteralPath $packagePath).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Preview root cannot be a link' }
foreach ($entry in Get-ChildItem -LiteralPath $packagePath -Recurse -Force) {
    if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Preview cannot contain links' }
}
$manifestPath = Join-Path $packagePath 'manifest.json'
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
$isPreview = $manifest.schema -eq 'livehub-rust-preview/1' -and $manifest.stage -eq 'migration-preview'
$isRelease = $manifest.schema -eq 'livehub-rust-release/1' -and $manifest.stage -eq 'release'
if ((-not $isPreview -and -not $isRelease) -or $manifest.target -ne 'windows-x64') { throw 'Unexpected package manifest' }
if ($manifest.version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$') { throw 'Unexpected preview version' }
$allowedFields = @('schema','version','stage','sourceState','target','files')
foreach ($field in $manifest.PSObject.Properties.Name) { if ($field -notin $allowedFields) { throw 'Unexpected manifest metadata' } }
$hashLine = ((Get-Content -LiteralPath ($manifestPath+'.sha256') -Raw).Trim() -split '\s+')
if ($hashLine.Count -lt 2 -or $hashLine[0] -ne (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash.ToLowerInvariant() -or $hashLine[1] -ne 'manifest.json') { throw 'Manifest checksum mismatch' }
$expected = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$previousEntry = $null
foreach ($entry in @($manifest.files)) {
    if ([string]::IsNullOrWhiteSpace($entry.path)) { throw 'Empty preview entry path' }
    if ([IO.Path]::IsPathRooted($entry.path) -or $entry.path -match '^[A-Za-z]:' -or $entry.path.Contains('\')) { throw ('Preview entry must be a relative forward-slash path: '+$entry.path) }
    if (@($entry.path.Split('/') | Where-Object { $_ -eq '..' }).Count) { throw ('Preview entry contains a parent segment: '+$entry.path) }
    if ($null -ne $previousEntry -and [StringComparer]::Ordinal.Compare($previousEntry, $entry.path) -ge 0) { throw 'Preview manifest file list is not sorted deterministically' }
    $previousEntry = $entry.path
    if (-not $expected.Add($entry.path)) { throw 'Duplicate preview entry' }
    $filePath = [IO.Path]::GetFullPath((Join-Path $packagePath $entry.path))
    $relative = [IO.Path]::GetRelativePath($packagePath,$filePath)
    if ($relative.StartsWith('..') -or [IO.Path]::IsPathRooted($relative)) { throw 'Preview entry escaped package' }
    $file = Get-Item -LiteralPath $filePath
    if ($file.LinkType -or $file.Length -ne $entry.size) { throw ('Preview file size/type mismatch: '+$entry.path) }
    if ((Get-FileHash -LiteralPath $filePath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $entry.sha256) { throw ('Preview checksum mismatch: '+$entry.path) }
}
$requiredEntries = @(
    'live-hub-rust.exe', 'LICENSE', 'README.md', 'THIRD_PARTY_NOTICES.md', 'Cargo.lock',
    'licenses/SOURCES.json', 'licenses/rust/SOURCES.json', 'licenses/upstream/SOURCES.json',
    'assets/fonts/LICENSE.txt',
    'assets/lua/live-title.lua',
    'assets/lua/live-resync.lua',
    'assets/emoji-sheet.png',
    'assets/emoji-sheet.txt'
)
foreach ($required in $requiredEntries) {
    if (-not $expected.Contains($required)) { throw ('Required preview entry missing: '+$required) }
}
if (-not @($expected | Where-Object { $_ -match '^assets/fonts/[^/]+\.ttf$' }).Count) { throw 'Embedded font file is missing from preview' }
$allFiles = @(Get-ChildItem -LiteralPath $packagePath -Recurse -File -Force)
foreach ($file in $allFiles) {
    $relative = [IO.Path]::GetRelativePath($packagePath,$file.FullName).Replace('\','/')
    if ($relative -in @('manifest.json','manifest.json.sha256')) { continue }
    if (-not $expected.Contains($relative)) { throw ('Unlisted preview file: '+$relative) }
    if ($relative -match '^(data|runtime|logs|profiles|userdata|sessions?|storage|backups?)/|(^|/)\.env([./]|$)|\.dpapi$|(^|/)cookies?([.-][^/]*)?$|(^|/)cookies\.(json|txt|sqlite|db)$|(^|/)login[ _-]?data|(^|/)logins\.json$|(^|/)key[34]\.db$|(^|/)local storage/|(^|/)credentials\.json$|(^|/)tokens?\.json$|\.(pfx|p12|pem|key)$') {
        throw ('User data or credential file in preview: '+$relative)
    }
}
# Check actual bytes, including UTF-16 strings inside the executable. Original
# third-party notices are retained; a finding is reported, never silently erased.
$privatePrefixes = @($env:USERPROFILE, $env:LOCALAPPDATA, $env:APPDATA, $env:TEMP, (Split-Path -Parent $PSScriptRoot), $ToolchainRoot) | Where-Object { $_ }
$privatePrefixes += $AdditionalPrivatePrefix
$needles = @($privatePrefixes | ForEach-Object { $_; $_.Replace('\','/'); $_.Replace('/','\') } | Sort-Object -Unique)
$findings = @()
foreach ($file in $allFiles) {
    $bytes = [IO.File]::ReadAllBytes($file.FullName)
    $views = @([Text.Encoding]::UTF8.GetString($bytes), [Text.Encoding]::Unicode.GetString($bytes))
    if ($bytes.Length -gt 1) { $views += [Text.Encoding]::Unicode.GetString($bytes,1,$bytes.Length-1) }
    foreach ($needle in $needles) {
        if ($views.Where({$_.IndexOf($needle,[StringComparison]::OrdinalIgnoreCase) -ge 0}).Count) {
            $findings += [IO.Path]::GetRelativePath($packagePath,$file.FullName).Replace('\','/')
            break
        }
    }
}
if ($findings.Count) { throw ('Private development path found in: '+($findings -join ', ')) }
[pscustomobject]@{
    FilesChecked=$allFiles.Count
    Entries=$expected.Count
    ManifestMatches=$true
    SortedManifest=$true
    RequiredEntries=$requiredEntries.Count
    PrivatePathFindings=0
    Scope='Local preview file hashes, deterministic ordering, required notices/font, path traversal, links, credential filenames and known host-path strings; no remote publication performed'
} | ConvertTo-Json
