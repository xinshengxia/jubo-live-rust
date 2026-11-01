[CmdletBinding(PositionalBinding = $false)]
param(
    [string]$ToolchainRoot = $env:LIVE_HUB_RUST_TOOLCHAIN,
    # Use a dedicated target directory when several work copies share one toolchain
    # cache: Cargo reuses same-name artifacts across copies, so a shared cache can
    # silently serve stale test binaries from another checkout.
    [string]$TargetDirectory,
    [Parameter(Position = 0, ValueFromRemainingArguments = $true)] [string[]]$CargoArguments = @('test', '--locked')
)
$ErrorActionPreference = 'Stop'
$repoPath = Split-Path -Parent $PSScriptRoot
$savedPath = $env:PATH
$savedCargo = $env:CARGO_HOME
$savedRustup = $env:RUSTUP_HOME
$savedTarget = $env:CARGO_TARGET_DIR
$savedTemp = $env:TEMP
$savedTmp = $env:TMP

# GNU ld on the x86_64-pc-windows-gnu target cannot open output paths that are not
# pure ASCII (it reports "cannot find *.o" or "cannot open output file" with GBK
# bytes). Reject such a target directory with an actionable message before Cargo
# spends minutes on a build that must fail.
function Assert-AsciiTargetDirectory([string]$TargetDirectory) {
    if (-not $TargetDirectory) { return }
    if ($TargetDirectory -match '[^\x00-\x7F]') {
        throw ("Cargo target directory must be an ASCII-only path: '$TargetDirectory'. " +
            "GNU ld cannot open non-ASCII output paths (it reports 'cannot find *.o' or " +
            "'cannot open output file'). Pass -TargetDirectory with an ASCII path, for example " +
            "'D:\build\live-hub-rust'. Worktrees under a Chinese repository path must not use " +
            "their own 'target' directory.")
    }
}

try {
    if ($ToolchainRoot) {
        $toolPath = [IO.Path]::GetFullPath($ToolchainRoot)
        $env:CARGO_HOME = Join-Path $toolPath 'cargo'
        $env:RUSTUP_HOME = Join-Path $toolPath 'rustup'
        # Keep build intermediates in the explicitly supplied tool cache, not the
        # source tree, and never fall back to a non-ASCII worktree path.
        $resolvedTarget = if ($TargetDirectory) { [IO.Path]::GetFullPath($TargetDirectory) } else { Join-Path $toolPath 'build/live-hub-rust' }
        Assert-AsciiTargetDirectory $resolvedTarget
        $env:CARGO_TARGET_DIR = $resolvedTarget
        $temporaryPath = Join-Path $toolPath 'tmp/live-hub-rust'
        New-Item -ItemType Directory -Path $temporaryPath -Force | Out-Null
        $env:TEMP = $temporaryPath
        $env:TMP = $temporaryPath
        $compilerBin = Join-Path $toolPath 'rustup/toolchains/stable-x86_64-pc-windows-gnu/bin'
        $cargoPath = Join-Path $compilerBin 'cargo.exe'
        if (-not (Test-Path -LiteralPath $cargoPath)) { throw 'Isolated Cargo is missing; provide a valid -ToolchainRoot.' }
        $env:PATH = $compilerBin + ';' + (Join-Path $toolPath 'w64devkit/bin') + ';' + $env:PATH
    } else {
        if ($TargetDirectory) {
            $resolvedTarget = [IO.Path]::GetFullPath($TargetDirectory)
            Assert-AsciiTargetDirectory $resolvedTarget
            $env:CARGO_TARGET_DIR = $resolvedTarget
        }
        $cargoPath = (Get-Command cargo -ErrorAction Stop).Source
    }
    Push-Location -LiteralPath $repoPath
    try {
        & $cargoPath @CargoArguments
        if ($LASTEXITCODE -ne 0) { throw "Cargo exited with $LASTEXITCODE" }
    } finally { Pop-Location }
} finally {
    $env:PATH = $savedPath
    $env:CARGO_HOME = $savedCargo
    $env:RUSTUP_HOME = $savedRustup
    $env:CARGO_TARGET_DIR = $savedTarget
    $env:TEMP = $savedTemp
    $env:TMP = $savedTmp
}
