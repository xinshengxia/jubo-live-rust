[CmdletBinding()]
param(
    [ValidateSet('lite', 'full', 'both')]
    [string]$Variant = 'both',
    # Optional already-built preview directory. When omitted, New-Preview.ps1
    # builds a fresh verified preview into a temporary staging directory.
    [string]$PreviewDirectory = '',
    [string]$ToolchainRoot = $env:LIVE_HUB_RUST_TOOLCHAIN,
    [string]$TargetDirectory = '',
    [string]$OutputDirectory = '',
    # Full package: directory containing mpv.exe (user-supplied portable mpv).
    [string]$MpvDirectory = '',
    # Optional explicit mpv license files (default: MpvDirectory\licenses or
    # MpvDirectory\LICENSE*).
    [string[]]$MpvLicenseFile = @(),
    [switch]$AllowPotPlayer,
    [switch]$AllowYtDlp,
    # Code signing: user-supplied certificate. Without these the package is
    # explicitly reported as unsigned and is never faked.
    [string]$SignTool = '',
    [string]$CertificateThumbprint = '',
    [string]$CertificatePfx = '',
    [string]$TimestampUrl = '',
    [switch]$RequireSignature,
    # Fixture tests for the gates (no package output).
    [switch]$SelfTest
)
$ErrorActionPreference = 'Stop'
$repoPath = Split-Path -Parent $PSScriptRoot
$releaseSchema = 'livehub-rust-release/1'
$forbiddenCredentialPath = '^(data|runtime|logs|profiles|userdata|sessions?|storage|backups?)/|(^|/)\.env([./]|$)|\.dpapi$|(^|/)cookies?([.-][^/]*)?$|(^|/)cookies\.(json|txt|sqlite|db)$|(^|/)login[ _-]?data|(^|/)logins\.json$|(^|/)key[34]\.db$|(^|/)local storage/|(^|/)credentials\.json$|(^|/)tokens?\.json$|\.(pfx|p12|pem|key)$'

function Write-Step([string]$Message) {
    Write-Host ('== ' + $Message) -ForegroundColor Cyan
}

function Get-Sha256([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-PackageFiles([string]$Root) {
    $rootFull = [IO.Path]::GetFullPath($Root)
    $rows = @()
    foreach ($file in Get-ChildItem -LiteralPath $rootFull -Recurse -File -Force) {
        $relative = [IO.Path]::GetRelativePath($rootFull, $file.FullName).Replace('\', '/')
        if ([IO.Path]::IsPathRooted($relative) -or $relative -match '(^|/)\.\.(/|$)') {
            throw ('package entry escaped the package root: ' + $relative)
        }
        $rows += [pscustomobject]@{ relative = $relative; full = $file.FullName; size = $file.Length }
    }
    return $rows
}

function Assert-TreeHasNoLinks([string]$Root) {
    $rootItem = Get-Item -LiteralPath $Root
    if ($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw ('package root is a link: ' + $Root) }
    foreach ($entry in Get-ChildItem -LiteralPath $Root -Recurse -Force) {
        if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw ('package contains a link or reparse point: ' + $entry.FullName)
        }
    }
}

function Copy-Tree([string]$Source, [string]$Destination) {
    New-Item -ItemType Directory -Path $Destination -Force | Out-Null
    foreach ($entry in Get-ChildItem -LiteralPath $Source -Force) {
        $target = Join-Path $Destination $entry.Name
        if ($entry.PSIsContainer) { Copy-Tree $entry.FullName $target }
        else { Copy-Item -LiteralPath $entry.FullName -Destination $target -Force }
    }
}

function Read-Version {
    $line = Get-Content -LiteralPath (Join-Path $repoPath 'Cargo.toml') | Where-Object { $_ -match '^version\s*=' } | Select-Object -First 1
    $match = [regex]::Match($line, '"([^"]+)"')
    if (-not $match.Success) { throw 'Cargo.toml package version is missing' }
    return $match.Groups[1].Value
}
function Get-PrivateNeedles([string[]]$AdditionalPrefix) {
    $prefixes = @($env:USERPROFILE, $env:LOCALAPPDATA, $env:APPDATA, $env:TEMP, (Split-Path -Parent $PSScriptRoot), $ToolchainRoot) |
        Where-Object { $_ }
    $prefixes += $AdditionalPrefix
    return @($prefixes | ForEach-Object { $_; $_.Replace('\', '/'); $_.Replace('/', '\') } | Sort-Object -Unique)
}

function Find-PrivateNeedle([string]$Path, [string[]]$Needles) {
    $length = (Get-Item -LiteralPath $Path).Length
    if ($length -le 8388608) {
        $bytes = [IO.File]::ReadAllBytes($Path)
        $views = @([Text.Encoding]::UTF8.GetString($bytes))
        if ($bytes.Length -gt 1) {
            $views += [Text.Encoding]::Unicode.GetString($bytes)
            $views += [Text.Encoding]::Unicode.GetString($bytes, 1, $bytes.Length - 1)
        }
        foreach ($needle in $Needles) {
            if ($needle.Length -lt 4) { continue }
            if ($views.Where({ $_.IndexOf($needle, [StringComparison]::OrdinalIgnoreCase) -ge 0 }).Count) { return $true }
        }
        return $false
    }
    # Large third-party player binaries are scanned in bounded chunks with a
    # carry-over tail so memory stays flat. UTF-16 is checked on even pairs and
    # one-offset pairs; a straddling match is still caught by the overlap.
    $chunkSize = 4 * 1024 * 1024
    $overlap = 1024
    $buffer = New-Object byte[] ($chunkSize + $overlap)
    $stream = [IO.File]::OpenRead($Path)
    try {
        $carry = 0
        while ($true) {
            $read = $stream.Read($buffer, $carry, $chunkSize)
            if ($read -le 0) { break }
            $usable = $carry + $read
            $even = $usable - ($usable % 2)
            $utf8 = [Text.Encoding]::UTF8.GetString($buffer, 0, $usable)
            $utf16Even = [Text.Encoding]::Unicode.GetString($buffer, 0, $even)
            $utf16Odd = if ($usable -gt 1) { [Text.Encoding]::Unicode.GetString($buffer, 1, $usable - 1 - (($usable - 1) % 2)) } else { '' }
            foreach ($needle in $Needles) {
                if ($needle.Length -lt 4) { continue }
                if ($utf8.IndexOf($needle, [StringComparison]::OrdinalIgnoreCase) -ge 0) { return $true }
                if ($utf16Even.IndexOf($needle, [StringComparison]::OrdinalIgnoreCase) -ge 0) { return $true }
                if ($utf16Odd.IndexOf($needle, [StringComparison]::OrdinalIgnoreCase) -ge 0) { return $true }
            }
            $carry = [Math]::Min($overlap, $usable)
            [Array]::Copy($buffer, $usable - $carry, $buffer, 0, $carry)
        }
    } finally { $stream.Dispose() }
    return $false
}

function Test-PrivatePackageContent([string]$Root, [string[]]$AdditionalPrefix = @()) {
    $needles = Get-PrivateNeedles $AdditionalPrefix
    $findings = @()
    foreach ($file in Get-PackageFiles $Root) {
        if ($file.relative -match $forbiddenCredentialPath) {
            $findings += ($file.relative + ' (credential/user-data filename)')
            continue
        }
        if (Find-PrivateNeedle $file.full $needles) {
            $findings += ($file.relative + ' (private development path in content)')
        }
    }
    return $findings
}

function Resolve-SignTool([string]$Explicit) {
    if ($Explicit) {
        if (-not (Test-Path -LiteralPath $Explicit -PathType Leaf)) { throw ('-SignTool not found: ' + $Explicit) }
        return $Explicit
    }
    $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    $kitRoots = @($env:ProgramFiles, [Environment]::GetEnvironmentVariable('ProgramFiles(x86)')) |
        Where-Object { $_ } | ForEach-Object { Join-Path $_ 'Windows Kits/10/bin' } |
        Where-Object { Test-Path -LiteralPath $_ }
    foreach ($kitRoot in $kitRoots) {
        $candidate = Get-ChildItem -LiteralPath $kitRoot -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending |
            ForEach-Object { Join-Path $_.FullName 'x64/signtool.exe' } |
            Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
        if ($candidate) { return $candidate }
    }
    return ''
}

function Invoke-SignatureGate([string]$Binary) {
    $status = [ordered]@{
        status = 'unsigned'; tool = ''; certificate = ''; verified = $false; timestamped = $false; notes = @()
    }
    $tool = Resolve-SignTool $SignTool
    $status.tool = $tool
    if ($tool -and (Test-Path -LiteralPath $Binary -PathType Leaf)) {
        $verify = & $tool verify /pa /q $Binary 2>&1
        if ($LASTEXITCODE -eq 0) {
            $status.verified = $true
            $status.notes += '现有二进制签名通过 signtool verify /pa'
        } else {
            $status.notes += '二进制当前未签名（signtool verify /pa 未通过），将按未签名发布'
        }
    } elseif (-not $tool) {
        $status.notes += '未找到 signtool.exe：需要安装 Windows SDK 的签名工具'
    }
    $certificate = $CertificateThumbprint
    if (-not $certificate -and $CertificatePfx) { $certificate = 'pfx:' + (Split-Path -Leaf $CertificatePfx) }
    if ($CertificateThumbprint -or $CertificatePfx) {
        if (-not $tool) { throw '提供了签名证书但找不到 signtool.exe；请安装 Windows SDK 或传 -SignTool' }
        $arguments = @('sign', '/fd', 'sha256')
        if ($CertificatePfx) {
            $password = [Environment]::GetEnvironmentVariable('LIVE_HUB_PFX_PASSWORD', 'Process')
            if (-not $password) { throw '使用 -CertificatePfx 时必须提供环境变量 LIVE_HUB_PFX_PASSWORD' }
            $arguments += @('/f', $CertificatePfx, '/p', $password)
        } else {
            $arguments += @('/sha1', $CertificateThumbprint)
        }
        if ($TimestampUrl) { $arguments += @('/tr', $TimestampUrl, '/td', 'sha256') }
        $arguments += $Binary
        & $tool @arguments 2>&1 | Out-Host
        if ($LASTEXITCODE -ne 0) { throw ('代码签名失败，signtool 退出码 ' + $LASTEXITCODE) }
        $status.status = 'signed'
        $status.certificate = $certificate
        $status.timestamped = [bool]$TimestampUrl
        & $tool verify /pa /q $Binary 2>&1 | Out-Host
        if ($LASTEXITCODE -ne 0) { throw '签名后 signtool verify /pa 未通过' }
        $status.verified = $true
    } else {
        $status.notes += '未提供证书：请用 -CertificateThumbprint <Cert:\CurrentUser\My 中自备代码签名证书指纹> 或 -CertificatePfx <pfx 路径>，并通过环境变量 LIVE_HUB_PFX_PASSWORD 提供密码；本机还需安装 Windows SDK signtool。本次按未签名如实发布，不伪造签名。'
    }
    if ($status.status -ne 'signed' -and $RequireSignature) {
        throw '要求 -RequireSignature，但没有可用的代码签名证书；拒绝生成未签名发布包'
    }
    return $status
}
function Assert-NoForbiddenPlayers([string]$Root, [switch]$AllowPotPlayer, [switch]$AllowYtDlp) {
    $forbidden = @()
    foreach ($file in Get-PackageFiles $Root) {
        if (-not $AllowPotPlayer -and $file.relative -match '(^|/)PotPlayer[^/]*\.exe$') { $forbidden += $file.relative }
        if (-not $AllowYtDlp -and $file.relative -match '(^|/)yt-dlp(\.exe)?$') { $forbidden += $file.relative }
    }
    if ($forbidden.Count) {
        throw ('发布包默认不允许捆绑 PotPlayer / yt-dlp（需 -AllowPotPlayer / -AllowYtDlp 显式开关）：' + ($forbidden -join ', '))
    }
}

function Add-MpvBundle([string]$PackageRoot, [string]$MpvSource, [string[]]$LicenseFiles) {
    if (-not $MpvSource) { throw 'full 包需要 -MpvDirectory 指向用户自备的 mpv 目录（含 mpv.exe）' }
    $source = [IO.Path]::GetFullPath($MpvSource)
    if (-not (Test-Path -LiteralPath $source -PathType Container)) { throw ('-MpvDirectory 不存在: ' + $source) }
    Assert-TreeHasNoLinks $source
    Assert-NoForbiddenPlayers $source -AllowPotPlayer:$AllowPotPlayer -AllowYtDlp:$AllowYtDlp
    $mpvExe = Join-Path $source 'mpv.exe'
    if (-not (Test-Path -LiteralPath $mpvExe -PathType Leaf)) { throw ('-MpvDirectory 缺少 mpv.exe: ' + $source) }
    $destination = Join-Path $PackageRoot 'bin/mpv'
    New-Item -ItemType Directory -Path (Join-Path $destination 'licenses') -Force | Out-Null
    foreach ($name in @('mpv.exe', 'mpv.com', 'd3dcompiler_43.dll')) {
        $candidate = Join-Path $source $name
        if (Test-Path -LiteralPath $candidate -PathType Leaf) { Copy-Item -LiteralPath $candidate -Destination $destination }
    }
    foreach ($dll in Get-ChildItem -LiteralPath $source -File -Filter '*.dll' -ErrorAction SilentlyContinue) {
        Copy-Item -LiteralPath $dll.FullName -Destination $destination -Force
    }
    $licenseCopied = 0
    if ($LicenseFiles.Count) {
        foreach ($file in $LicenseFiles) {
            $resolved = [IO.Path]::GetFullPath($file)
            if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) { throw ('-MpvLicenseFile 不存在: ' + $file) }
            Copy-Item -LiteralPath $resolved -Destination (Join-Path $destination 'licenses') -Force
            $licenseCopied++
        }
    } else {
        $licenseDirectory = Join-Path $source 'licenses'
        if (Test-Path -LiteralPath $licenseDirectory -PathType Container) {
            Copy-Tree $licenseDirectory (Join-Path $destination 'licenses')
            $licenseCopied = @(Get-ChildItem -LiteralPath (Join-Path $destination 'licenses') -Recurse -File).Count
        } else {
            $candidates = @(Get-ChildItem -LiteralPath $source -File -ErrorAction SilentlyContinue |
                Where-Object { $_.Name -match '^(LICEN[SC]E|COPYING|README)' })
            if (-not $candidates.Count) {
                throw '找不到 mpv 许可文件：请在 -MpvDirectory 下提供 licenses/，或用 -MpvLicenseFile 显式指定 mpv 的许可原文'
            }
            foreach ($file in $candidates) {
                Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $destination 'licenses')
                $licenseCopied++
            }
        }
    }
    if ($licenseCopied -eq 0) { throw 'mpv 许可文件未能复制，拒绝生成 full 包' }
    $notice = @(
        '# 第三方播放器说明：mpv',
        '',
        '本包 bin/mpv/ 下为第三方 mpv 播放器（按用户自备目录打包，本仓库不再分发）。',
        'mpv 的许可原文随包保存在 bin/mpv/licenses/ 下；其源代码与构建说明见 mpv 官方项目。',
        '本程序只以独立进程方式调用 mpv，不修改、不重链接其代码。',
        '播放器版本与构建信息由打包时提供的 mpv 目录决定。'
    )
    Set-Content -LiteralPath (Join-Path $destination 'THIRD-PARTY-NOTICE.txt') -Value $notice -Encoding utf8
}

function Write-ReleaseManifest([string]$PackageRoot, [string]$Version, [string]$PackageVariant, [System.Collections.Specialized.OrderedDictionary]$Signature, [hashtable]$Gates) {
    $files = @(Get-PackageFiles $PackageRoot | Where-Object { $_.relative -notin @('release-manifest.json', 'release-manifest.json.sha256') })
    $byRelative = @{}
    foreach ($file in $files) { $byRelative[$file.relative] = $file }
    $paths = [string[]]@($byRelative.Keys)
    [Array]::Sort($paths, [StringComparer]::Ordinal)
    $entries = @(foreach ($relative in $paths) {
        $file = $byRelative[$relative]
        [ordered]@{ path = $relative; size = $file.size; sha256 = (Get-Sha256 $file.full) }
    })
    $manifest = [ordered]@{
        schema = $releaseSchema
        stage = 'release'
        variant = $PackageVariant
        version = $Version
        target = 'windows-x64'
        signature = [ordered]@{ status = $Signature.status; verified = $Signature.verified; timestamped = $Signature.timestamped }
        gates = $Gates
        files = $entries
    }
    $manifestPath = Join-Path $PackageRoot 'release-manifest.json'
    $manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $manifestPath -Encoding utf8
    $hash = Get-Sha256 $manifestPath
    ($hash + '  release-manifest.json') | Set-Content -LiteralPath (Join-Path $PackageRoot 'release-manifest.json.sha256') -Encoding ascii
    return @{ manifest = $manifest; hash = $hash; entries = $entries }
}

function New-ReleaseZip([string]$PackageRoot, [string]$ZipPath) {
    $files = @(Get-PackageFiles $PackageRoot)
    $byRelative = @{}
    foreach ($file in $files) { $byRelative[$file.relative] = $file }
    $paths = [string[]]@($byRelative.Keys)
    [Array]::Sort($paths, [StringComparer]::Ordinal)
    if (Test-Path -LiteralPath $ZipPath) { Remove-Item -LiteralPath $ZipPath -Force }
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::Open($ZipPath, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($relative in $paths) {
            $entry = $archive.CreateEntry($relative, [System.IO.Compression.CompressionLevel]::Optimal)
            $input = [IO.File]::OpenRead($byRelative[$relative].full)
            try {
                $output = $entry.Open()
                try { $input.CopyTo($output) } finally { $output.Dispose() }
            } finally { $input.Dispose() }
        }
    } finally { $archive.Dispose() }
}

function Test-ReleaseZip([string]$ZipPath, [string]$PackageRoot) {
    $manifestPath = Join-Path $PackageRoot 'release-manifest.json'
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    $expected = @{}
    foreach ($entry in $manifest.files) { $expected[$entry.path] = $entry }
    $seen = @{}
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($ZipPath)
    try {
        foreach ($entry in $archive.Entries) {
            $name = $entry.FullName
            if ([IO.Path]::IsPathRooted($name) -or $name -match '(^|/)\.\.(/|$)' -or $name.Contains('\')) {
                throw ('zip entry is not a safe relative forward-slash path: ' + $name)
            }
            if ($seen.ContainsKey($name)) { throw ('duplicate zip entry: ' + $name) }
            $seen[$name] = $true
            $stream = $entry.Open()
            try {
                $sha = [Security.Cryptography.SHA256]::Create()
                try { $hash = ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-', '').ToLowerInvariant() } finally { $sha.Dispose() }
            } finally { $stream.Dispose() }
            if ($name -eq 'release-manifest.json' -or $name -eq 'release-manifest.json.sha256') { continue }
            if (-not $expected.ContainsKey($name)) { throw ('zip entry is not listed in release manifest: ' + $name) }
            if ($expected[$name].size -ne $entry.Length) { throw ('zip entry size mismatch: ' + $name) }
            if ($expected[$name].sha256 -ne $hash) { throw ('zip entry checksum mismatch: ' + $name) }
        }
        if ($seen.Count -ne ($expected.Count + 2)) {
            throw ('zip entry count mismatch: got ' + $seen.Count + ', expected ' + ($expected.Count + 2))
        }
    } finally { $archive.Dispose() }
    $manifestHash = Get-Sha256 $manifestPath
    $declared = ((Get-Content -LiteralPath (Join-Path $PackageRoot 'release-manifest.json.sha256') -Raw).Trim() -split '\s+')[0]
    if ($manifestHash -ne $declared) { throw 'release manifest checksum file does not match the manifest' }
    return [ordered]@{ entries = $seen.Count; manifestSha256 = $manifestHash; listedFiles = $expected.Count }
}
function New-PackageFromPreview([string]$PreviewPath, [string]$PackageVariant, [string]$StagingRoot, [string]$Version) {
    $packageRoot = Join-Path $StagingRoot ('package-' + $PackageVariant)
    Copy-Tree $PreviewPath $packageRoot
    if ($PackageVariant -eq 'full') { Add-MpvBundle $packageRoot $MpvDirectory $MpvLicenseFile }
    Assert-TreeHasNoLinks $packageRoot
    Assert-NoForbiddenPlayers $packageRoot -AllowPotPlayer:$AllowPotPlayer -AllowYtDlp:$AllowYtDlp
    $binary = Join-Path $packageRoot 'live-hub-rust.exe'
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw '打包内容缺少 live-hub-rust.exe' }
    # 便携标记：与 C# 绿色包语义一致（full/lite 都按便携布局运行，数据写包内
    # data\ 而不是 %LOCALAPPDATA%）；full 还靠 bin/mpv/mpv.exe 判定，lite 只有它。
    Set-Content -LiteralPath (Join-Path $packageRoot 'portable.marker') -Value ('livehub-rust ' + $PackageVariant + ' portable package') -Encoding ascii
    Write-Step ('便携布局离线自检: ' + $PackageVariant)
    $selfTestOut = Join-Path $StagingRoot ('selftest-' + $PackageVariant + '.out')
    $selfTestErr = Join-Path $StagingRoot ('selftest-' + $PackageVariant + '.err')
    $selfTest = Start-Process -WindowStyle Hidden -FilePath $binary -ArgumentList 'self-test' -Wait -PassThru -RedirectStandardOutput $selfTestOut -RedirectStandardError $selfTestErr
    if ($selfTest.ExitCode -ne 0) {
        $detail = [string](Get-Content -LiteralPath $selfTestOut -Raw) + [string](Get-Content -LiteralPath $selfTestErr -Raw)
        throw ('便携布局离线自检失败：' + $detail)
    }
    Write-Step ('签名门禁: ' + $PackageVariant)
    $signature = Invoke-SignatureGate $binary
    Write-Step ('隐私门禁: ' + $PackageVariant)
    $findings = @(Test-PrivatePackageContent $packageRoot)
    if ($findings.Count) { throw ('发现隐私/用户数据内容，拒绝打包：' + ($findings -join '; ')) }
    $gates = @{
        privatePathFindings = 0
        credentialNameFindings = 0
        links = 'none'
        forbiddenPlayers = 'none'
        portableSelfTest = 'ok'
        signatureStatus = $signature.status
    }
    $manifest = Write-ReleaseManifest $packageRoot $Version $PackageVariant $signature $gates
    $zipName = 'live-hub-rust-' + $Version + '-' + $PackageVariant + '.zip'
    $zipPath = Join-Path $OutputDirectory $zipName
    Write-Step ('生成 ZIP: ' + $zipName)
    New-ReleaseZip $packageRoot $zipPath
    Write-Step ('校验 ZIP: ' + $zipName)
    $zipReport = Test-ReleaseZip $zipPath $packageRoot
    $zipHash = Get-Sha256 $zipPath
    ($zipHash + '  ' + $zipName) | Set-Content -LiteralPath ($zipPath + '.sha256') -Encoding ascii
    return [ordered]@{
        variant = $PackageVariant
        file = $zipPath
        bytes = (Get-Item -LiteralPath $zipPath).Length
        sha256 = $zipHash
        files = $manifest.entries.Count
        manifestSha256 = $manifest.hash
        zipEntries = $zipReport.entries
        signature = $signature
        privacyFindings = 0
    }
}

function Invoke-PublishSelfTest {
    $root = Join-Path $env:TEMP ('livehub-publish-selftest-' + [Guid]::NewGuid().ToString('N'))
    $script:results = @()
    $add = {
        param([string]$Name, [bool]$Ok, [string]$Detail)
        $script:results += [pscustomobject]@{ name = $Name; ok = $Ok; detail = $Detail }
    }
    New-Item -ItemType Directory -Path $root -Force | Out-Null
    try {
        $clean = Join-Path $root 'clean'
        New-Item -ItemType Directory -Path $clean -Force | Out-Null
        Set-Content -LiteralPath (Join-Path $clean 'live-hub-rust.exe') -Value 'synthetic binary' -Encoding ascii
        Set-Content -LiteralPath (Join-Path $clean 'README.md') -Value 'public readme' -Encoding utf8
        $dummySignature = [ordered]@{ status = 'unsigned'; verified = $false; timestamped = $false }
        $manifest = Write-ReleaseManifest $clean '0.0.0-selftest' 'lite' $dummySignature @{ privacyFindings = 0 }
        $zip = Join-Path $root 'clean.zip'
        New-ReleaseZip $clean $zip
        $zipReport = Test-ReleaseZip $zip $clean
        & $add 'manifest-and-zip-roundtrip' ($zipReport.listedFiles -eq 2 -and $manifest.entries.Count -eq 2) ('entries=' + $zipReport.entries)

        $privatePackage = Join-Path $root 'private'
        Copy-Tree $clean $privatePackage
        Set-Content -LiteralPath (Join-Path $privatePackage 'notes.txt') -Value ('build root ' + $env:USERPROFILE) -Encoding utf8
        $findings = @(Test-PrivatePackageContent $privatePackage)
        & $add 'private-path-detected' ($findings.Count -gt 0) ($findings -join '; ')

        $credentialPackage = Join-Path $root 'credential'
        Copy-Tree $clean $credentialPackage
        Set-Content -LiteralPath (Join-Path $credentialPackage 'cookies.txt') -Value 'SESSION=synthetic' -Encoding ascii
        $findings = @(Test-PrivatePackageContent $credentialPackage)
        & $add 'credential-name-detected' ($findings.Count -gt 0) ($findings -join '; ')

        $playerPackage = Join-Path $root 'player'
        Copy-Tree $clean $playerPackage
        Set-Content -LiteralPath (Join-Path $playerPackage 'PotPlayerMini64.exe') -Value 'synthetic' -Encoding ascii
        $refused = $false
        try { Assert-NoForbiddenPlayers $playerPackage } catch { $refused = $true }
        & $add 'forbidden-player-refused-by-default' $refused 'PotPlayerMini64.exe bundle refused without -AllowPotPlayer'

        $signature = Invoke-SignatureGate (Join-Path $clean 'live-hub-rust.exe')
        & $add 'signature-gate-reports-unsigned' ($signature.status -eq 'unsigned' -and @($signature.notes).Count -gt 0) (@($signature.notes) -join '; ')

        $requiredRefused = $false
        try {
            $saved = $RequireSignature
            $script:RequireSignature = $true
            Invoke-SignatureGate (Join-Path $clean 'live-hub-rust.exe') | Out-Null
        } catch {
            if ($_.Exception.Message -match 'RequireSignature') { $requiredRefused = $true } else { throw }
        } finally {
            $script:RequireSignature = $false
        }
        & $add 'require-signature-fails-without-certificate' $requiredRefused 'explicit failure instead of a fake signature'
    } finally {
        Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
    }
    $failed = @($script:results | Where-Object { -not $_.ok }).Count
    [ordered]@{ stage = 'publish-selftest'; passed = $script:results.Count - $failed; failed = $failed; checks = $script:results } |
        ConvertTo-Json -Depth 6
    if ($failed -gt 0) { exit 1 }
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
if ($SelfTest) { Invoke-PublishSelfTest; exit 0 }
if (-not $OutputDirectory) { throw 'Provide -OutputDirectory for the release ZIPs.' }
$outputPath = [IO.Path]::GetFullPath($OutputDirectory)
if ((Test-Path -LiteralPath $outputPath) -and @(Get-ChildItem -LiteralPath $outputPath -Force).Count) {
    throw 'Release output must be a new or empty directory; existing files are retained.'
}
if (-not ($Variant -in @('lite', 'full')) -and $Variant -ne 'both') { throw 'Unsupported -Variant' }
$variants = @()
if ($Variant -eq 'both') { $variants = @('lite', 'full') } else { $variants = @($Variant) }
if ($variants -contains 'full' -and -not $MpvDirectory) { throw 'full 包需要 -MpvDirectory（用户自备 mpv 目录，含 mpv.exe 与许可文件）' }
New-Item -ItemType Directory -Path $outputPath -Force | Out-Null
$stagingRoot = Join-Path $env:TEMP ('live-hub-publish-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stagingRoot -Force | Out-Null
$startedAt = Get-Date
$summary = [ordered]@{
    schema = 'livehub-rust-publish/1'
    startedUtc = $startedAt.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    repo = $repoPath
    output = $outputPath
    variants = $variants
    preview = [ordered]@{ source = ''; verified = $false }
    packages = @()
    notes = @()
}
try {
    if ($PreviewDirectory) {
        $previewPath = [IO.Path]::GetFullPath($PreviewDirectory)
        if (-not (Test-Path -LiteralPath $previewPath -PathType Container)) { throw ('-PreviewDirectory 不存在: ' + $previewPath) }
        Assert-TreeHasNoLinks $previewPath
        if (-not $SkipPreviewVerify) {
            Write-Step '校验预览目录（哈希/隐私/凭据/私有路径）'
            & (Join-Path $PSScriptRoot 'Test-Preview.ps1') -Path $previewPath -ToolchainRoot $ToolchainRoot | Out-Null
            $summary.preview.verified = $true
        }
        $summary.preview.source = $previewPath
    } else {
        Write-Step '构建预览包（New-Preview.ps1）'
        $previewPath = Join-Path $stagingRoot 'preview'
        $buildTarget = $TargetDirectory
        if (-not $buildTarget -and $ToolchainRoot) { $buildTarget = Join-Path $ToolchainRoot 'build/live-hub-rust-publish' }
        $buildArguments = @{ OutputDirectory = $previewPath }
        if ($ToolchainRoot) { $buildArguments.ToolchainRoot = $ToolchainRoot }
        if ($buildTarget) { $buildArguments.TargetDirectory = $buildTarget }
        & (Join-Path $PSScriptRoot 'New-Preview.ps1') @buildArguments | Out-Null
        $summary.preview.source = $previewPath
        $summary.preview.verified = $true
    }
    $version = Read-Version
    $summary.version = $version
    $summary.buildTimeUtc = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    foreach ($packageVariant in $variants) {
        Write-Step ('打包 ' + $packageVariant)
        $package = New-PackageFromPreview $previewPath $packageVariant $stagingRoot $version
        $summary.packages += $package
        if ($package.signature.status -ne 'signed') {
            $summary.notes += ($packageVariant + ' 包未签名：用户需自备代码签名证书（-CertificateThumbprint/-CertificatePfx + LIVE_HUB_PFX_PASSWORD，并安装 Windows SDK signtool）')
        }
    }
    $summary.durationSeconds = [Math]::Round(((Get-Date) - $startedAt).TotalSeconds, 1)
    $summary | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $outputPath 'publish-summary.json') -Encoding utf8
    Write-Host ''
    foreach ($package in $summary.packages) {
        Write-Host ($package.variant + ' -> ' + $package.file + ' (' + [Math]::Round($package.bytes / 1MB, 2) + ' MB, files=' + $package.files + ', signature=' + $package.signature.status + ')') -ForegroundColor Green
    }
    Write-Host ('summary -> ' + (Join-Path $outputPath 'publish-summary.json')) -ForegroundColor Green
} finally {
    Remove-Item -LiteralPath $stagingRoot -Recurse -Force -ErrorAction SilentlyContinue
}