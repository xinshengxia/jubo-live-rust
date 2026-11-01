<#
.SYNOPSIS
  Capture reproducible Rust launcher screenshots through the program's own
  --ui-shot diagnostic.

.DESCRIPTION
  Runs the Rust launcher with an isolated synthetic data directory and asks its
  own renderer to save one PNG per launcher page (no desktop capture, no
  third-party screenshot tool). The launcher opens the window off-screen and
  inactive; this script additionally samples the Windows foreground window while
  the capture runs and fails if the capture process ever takes the foreground.

  Defaults are fully offline: no network, no external player, no real user data.
  The data directory is seeded only from scripts/fixtures/ui-shot-*.json.

.PARAMETER OutputDirectory
  Receives <page>.png, capture.json (written by the launcher) and
  capture-evidence.json (written by this script).

.PARAMETER DataDirectory
  Explicit isolated data root. When omitted, <OutputDirectory>/data is recreated
  on every run so the screenshots always show the same synthetic data.

.PARAMETER Pages
  Comma separated page keys, or all. Final keys: favorites,recent,lookup,
  sessions,players,danmaku,accounts,data.

.PARAMETER Size
  Logical content size, WIDTHxHEIGHT. Default 1240x720 (legacy window size).

.PARAMETER SkipBuild
  Reuse <TargetDirectory>/debug/live-hub-rust.exe instead of building first.

.EXAMPLE
  pwsh -NoProfile -File scripts/Capture-RustUi.ps1 -ToolchainRoot '<toolchain>' -TargetDirectory $env:LIVE_HUB_BUILD_DIR -OutputDirectory 'D:\captures\rust-ui'
#>
[CmdletBinding()]
param(
    [string]$ToolchainRoot = $env:LIVE_HUB_RUST_TOOLCHAIN,
    [string]$TargetDirectory = $env:LIVE_HUB_BUILD_DIR,
    [string]$Exe,
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [string]$DataDirectory,
    [string]$Pages = 'all',
    [string]$Size = '1240x720',
    [ValidateSet('', 'dark', 'light')][string]$Theme = '',
    [switch]$SkipBuild,
    [int]$TimeoutSeconds = 240
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null

$freshData = $false
if (-not $DataDirectory) {
    $DataDirectory = Join-Path $OutputDirectory 'data'
    $freshData = $true
}
$DataDirectory = [IO.Path]::GetFullPath($DataDirectory)
if ($freshData -and (Test-Path -LiteralPath $DataDirectory)) {
    Remove-Item -LiteralPath $DataDirectory -Recurse -Force
}
New-Item -ItemType Directory -Path $DataDirectory -Force | Out-Null

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class UiShotForeground {
    [DllImport("user32.dll")]
    public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern int GetWindowTextW(IntPtr hWnd, StringBuilder text, int count);
    public static string Title(IntPtr hWnd) {
        if (hWnd == IntPtr.Zero) { return ""; }
        var buffer = new StringBuilder(512);
        GetWindowTextW(hWnd, buffer, buffer.Capacity);
        return buffer.ToString();
    }
    public static uint OwnerPid(IntPtr hWnd) {
        uint pid;
        GetWindowThreadProcessId(hWnd, out pid);
        return pid;
    }
}
'@

function Write-Step([string]$message) {
    Write-Host ("[ui-shot] " + $message)
}

# 1) Build (default) or reuse an explicit executable.
$executable = $Exe
if ($executable) {
    $executable = [IO.Path]::GetFullPath($executable)
}
elseif (-not $SkipBuild) {
    Write-Step "building debug binary into $TargetDirectory"
    $buildScript = Join-Path $repoRoot 'scripts/Build.ps1'
    & $buildScript -ToolchainRoot $ToolchainRoot -TargetDirectory $TargetDirectory
    $executable = Join-Path ([IO.Path]::GetFullPath($TargetDirectory)) 'debug/live-hub-rust.exe'
}
else {
    $executable = Join-Path ([IO.Path]::GetFullPath($TargetDirectory)) 'debug/live-hub-rust.exe'
}
if (-not (Test-Path -LiteralPath $executable)) { throw "找不到可执行文件：$executable" }
Write-Step "exe: $executable"

# 2) Seed the isolated data root from synthetic fixtures only.
$favoritesFixture = Join-Path $PSScriptRoot 'fixtures/ui-shot-favorites.json'
$historyFixture = Join-Path $PSScriptRoot 'fixtures/ui-shot-history.json'
foreach ($fixture in @($favoritesFixture, $historyFixture)) {
    if (-not (Test-Path -LiteralPath $fixture)) { throw "缺少合成数据 fixture：$fixture" }
}
$seedLog = Join-Path $OutputDirectory 'seed.log'
$seedError = Join-Path $OutputDirectory 'seed-error.log'
function Invoke-Seed([string[]]$arguments) {
    $process = Start-Process -FilePath $executable -ArgumentList $arguments -Wait -PassThru -NoNewWindow -RedirectStandardOutput $seedLog -RedirectStandardError $seedError
    if ($process.ExitCode -ne 0) {
        $detail = ''
        if (Test-Path -LiteralPath $seedError) { $detail = (Get-Content -LiteralPath $seedError -Raw) }
        throw ("合成数据导入失败（exit " + $process.ExitCode + "）：" + $detail)
    }
}
Write-Step "seeding isolated data root: $DataDirectory"
Invoke-Seed @('--data-dir', $DataDirectory, 'import', $favoritesFixture)
Invoke-Seed @('--data-dir', $DataDirectory, 'import', $historyFixture, '--history')

# 3) Capture with foreground-window evidence.
$captureLog = Join-Path $OutputDirectory 'capture-stdout.log'
$captureError = Join-Path $OutputDirectory 'capture-stderr.log'
$before = [UiShotForeground]::GetForegroundWindow()
$arguments = @('--data-dir', $DataDirectory, '--ui-shot', $OutputDirectory, '--page', $Pages, '--size', $Size)
if ($Theme) { $arguments += @('--theme', $Theme) }
Write-Step ("capture: --ui-shot " + $OutputDirectory + " --page " + $Pages + " --size " + $Size + $(if ($Theme) { " --theme " + $Theme } else { '' }))
$started = Get-Date
$process = Start-Process -FilePath $executable -ArgumentList $arguments -PassThru -WindowStyle Hidden -RedirectStandardOutput $captureLog -RedirectStandardError $captureError
$samples = 0
$tookForeground = $false
$foregroundTitles = New-Object System.Collections.Generic.List[string]
$deadline = $started.AddSeconds($TimeoutSeconds)
while (-not $process.HasExited) {
    if ((Get-Date) -gt $deadline) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        throw "截图进程超过 $TimeoutSeconds 秒未退出，已终止"
    }
    Start-Sleep -Milliseconds 150
    $samples++
    $current = [UiShotForeground]::GetForegroundWindow()
    $ownerPid = [UiShotForeground]::OwnerPid($current)
    if ($ownerPid -eq $process.Id) {
        $tookForeground = $true
        $foregroundTitles.Add([UiShotForeground]::Title($current))
    }
}
$process.WaitForExit()
$process.Refresh()
$exitCode = $process.ExitCode
$after = [UiShotForeground]::GetForegroundWindow()
$foregroundUnchanged = ($before -eq $after)
$elapsed = ((Get-Date) - $started).TotalSeconds

# 4) Validate the launcher's own capture report.
$captureJsonPath = Join-Path $OutputDirectory 'capture.json'
$capture = $null
if (Test-Path -LiteralPath $captureJsonPath) {
    $rawCapture = Get-Content -LiteralPath $captureJsonPath -Raw
    if ($rawCapture) { $capture = $rawCapture | ConvertFrom-Json }
}
$missing = New-Object System.Collections.Generic.List[string]
if ($capture) {
    foreach ($page in @($capture.pages)) {
        $png = Join-Path $OutputDirectory $page.file
        if (-not (Test-Path -LiteralPath $png)) { $missing.Add($page.file) }
        elseif ((Get-Item -LiteralPath $png).Length -lt 1024) { $missing.Add($page.file + "（<1KiB）") }
    }
}
$captureOk = ($null -ne $capture) -and ($capture.ok -eq $true) -and ($missing.Count -eq 0)

# 5) Write portable evidence (never foreign window titles or absolute paths).
$evidence = [ordered]@{
    stage = 'rust-ui-capture'
    captured_at = (Get-Date).ToString('o')
    exe_name = (Split-Path -Leaf $executable)
    pages = $Pages
    size = $Size
    theme = $(if ($Theme) { $Theme } else { 'dark' })
    exit_code = $exitCode
    elapsed_seconds = [math]::Round($elapsed, 2)
    foreground = [ordered]@{
        samples = $samples
        capture_window_took_foreground = $tookForeground
        foreground_unchanged_before_after = $foregroundUnchanged
        capture_window_titles = @($foregroundTitles)
    }
    capture_ok = $captureOk
    missing_png = @($missing)
    capture = $capture
    logs = [ordered]@{
        capture_stdout = (Split-Path -Leaf $captureLog)
        capture_stderr = (Split-Path -Leaf $captureError)
        seed = (Split-Path -Leaf $seedLog)
    }
}
$evidencePath = Join-Path $OutputDirectory 'capture-evidence.json'
$evidence | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $evidencePath -Encoding utf8

$capturedCount = 0
if ($capture) { $capturedCount = @($capture.pages).Count }
Write-Step ("exit=" + $exitCode + " pages=" + $capturedCount + " ok=" + $captureOk + " foreground_taken=" + $tookForeground + " samples=" + $samples + " elapsed=" + [math]::Round($elapsed, 1) + "s")
Write-Step ("evidence: " + $evidencePath)

if ($exitCode -ne 0) {
    $stderrText = ''
    if (Test-Path -LiteralPath $captureError) { $stderrText = (Get-Content -LiteralPath $captureError -Raw) }
    throw ("截图进程失败（exit " + $exitCode + "）：" + $stderrText)
}
if (-not $captureOk) {
    throw ("截图报告不完整/失败：capture.json 缺失或 ok!=true，缺少 PNG：" + ($missing -join ', '))
}
if ($tookForeground) {
    throw "截图窗口抢到了前台（前台窗口 PID 曾等于截图进程 PID），不满足不抢前台约束"
}
Write-Step 'ok：截图完整且未抢前台'
