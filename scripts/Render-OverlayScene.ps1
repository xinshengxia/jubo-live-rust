# Offline render evidence for the danmaku/SC overlay.
#
# Creates an OFF-SCREEN synthetic player window (never activated, never takes the
# user's foreground), starts the overlay exe against it with an opt-in synthetic
# fixture (no network), captures the overlay window with PrintWindow at one or
# more player sizes, then tears everything down. Nothing is written outside the
# evidence dir and a temporary isolated data dir.
#
# Example:
#   pwsh -NoProfile -File scripts/Render-OverlayScene.ps1 -Exe <exe> -EvidenceDir <dir> -Fixture <fixture.json> -Sizes 960x540,1280x720 -WaitMs 4500 -StepMs 2500
param(
    [string]$Exe = '',
    [string]$EvidenceDir = '',
    [string]$Fixture = '',
    [string]$StateJson = '',
    [string]$Sizes = '960x540',
    [int]$WaitMs = 5000,
    [int]$StepMs = 2500,
    [int]$X = -6000,
    [int]$Y = 100,
    [string]$Prefix = 'scene',
    [string]$PlayerTitle = 'LiveHubScenePlayer',
    [int]$FreezeMs = 0,
    [switch]$PolicyCheck,
    [switch]$NoForceShow,
    [switch]$PlayerOnly
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$user32 = @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class SceneWin
{
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X; public int Y; }
    [StructLayout(LayoutKind.Sequential)] public struct MSG {
        public IntPtr hwnd; public uint message; public IntPtr wParam; public IntPtr lParam; public uint time; public POINT pt; public uint lPrivate;
    }
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)] public static extern IntPtr CreateWindowExW(uint dwExStyle, string lpClassName, string lpWindowName, uint dwStyle, int x, int y, int nWidth, int nHeight, IntPtr hWndParent, IntPtr hMenu, IntPtr hInstance, IntPtr lpParam);
    [DllImport("user32.dll")] public static extern bool DestroyWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr hWnd);
    [DllImport("user32.dll", SetLastError = true)] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int X, int Y, int cx, int cy, uint uFlags);
    [DllImport("user32.dll", SetLastError = true)] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll", SetLastError = true)] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr hWnd, StringBuilder lpString, int nMaxCount);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdcBlt, uint nFlags);
    [DllImport("user32.dll")] public static extern bool PeekMessageW(out MSG lpMsg, IntPtr hWnd, uint wMsgFilterMin, uint wMsgFilterMax, uint wRemoveMsg);
    [DllImport("user32.dll")] public static extern bool TranslateMessage(ref MSG lpMsg);
    [DllImport("user32.dll")] public static extern IntPtr DispatchMessageW(ref MSG lpMsg);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr hWnd, int nIndex);

    public static IntPtr[] WindowsOfProcess(uint pid)
    {
        List<IntPtr> found = new List<IntPtr>();
        EnumWindows(delegate(IntPtr h, IntPtr l) {
            uint owner = 0;
            GetWindowThreadProcessId(h, out owner);
            if (owner == pid) { found.Add(h); }
            return true;
        }, IntPtr.Zero);
        return found.ToArray();
    }
    public static string WindowText(IntPtr h) { StringBuilder sb = new StringBuilder(1024); GetWindowTextW(h, sb, sb.Capacity); return sb.ToString(); }
    public static RECT WindowRectangle(IntPtr h) { RECT r; GetWindowRect(h, out r); return r; }
    public static void Pump(int milliseconds)
    {
        int end = Environment.TickCount + milliseconds;
        MSG message;
        while (Environment.TickCount < end)
        {
            while (PeekMessageW(out message, IntPtr.Zero, 0, 0, 1)) { TranslateMessage(ref message); DispatchMessageW(ref message); }
            Thread.Sleep(10);
        }
    }
    public static IntPtr CreatePlayer(string title, int x, int y, int width, int height)
    {
        IntPtr handle = CreateWindowExW(0x80, "STATIC", title, 0x80000000, x, y, width, height, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero);
        if (handle == IntPtr.Zero) { throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error()); }
        ShowWindowAsync(handle, 4);
        return handle;
    }
}
'@
Add-Type -TypeDefinition $user32 -Language CSharp
Add-Type -AssemblyName System.Drawing

if ($PlayerOnly) {
    [void][SceneWin]::SetProcessDPIAware()
    $handle = [SceneWin]::CreatePlayer($PlayerTitle, -6100, 100, 960, 540)
    while ([SceneWin]::IsWindow($handle)) { [SceneWin]::Pump(200) }
    exit 0
}

function Save-Capture([IntPtr]$handle, [string]$path) {
    $rect = [SceneWin]::WindowRectangle($handle)
    $width = [math]::Max(1, $rect.Right - $rect.Left)
    $height = [math]::Max(1, $rect.Bottom - $rect.Top)
    $bitmap = New-Object System.Drawing.Bitmap -ArgumentList @($width, $height, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $hdc = $graphics.GetHdc()
    $ok = [SceneWin]::PrintWindow($handle, $hdc, 2)
    $graphics.ReleaseHdc($hdc)
    $graphics.Dispose()
    $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $greyPath = [System.IO.Path]::Combine([System.IO.Path]::GetDirectoryName($path), [System.IO.Path]::GetFileNameWithoutExtension($path) + '-grey.png')
    $rgb = New-Object System.Drawing.Bitmap -ArgumentList @($width, $height, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $g = [System.Drawing.Graphics]::FromImage($rgb)
    $g.Clear([System.Drawing.Color]::FromArgb(118, 122, 128))
    $g.DrawImage($bitmap, 0, 0, $width, $height)
    $g.Dispose()
    $rgb.Save($greyPath, [System.Drawing.Imaging.ImageFormat]::Png)
    $rgb.Dispose()
    $bitmap.Dispose()
    return [pscustomobject]@{ printwindow_ok = $ok; width = $width; height = $height; png = $path; grey_png = $greyPath }
}

$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $Exe) {
    if ($env:LIVE_HUB_ACCEPTANCE_OVERLAY_EXE) { $Exe = $env:LIVE_HUB_ACCEPTANCE_OVERLAY_EXE }
    else { $Exe = Join-Path $repoRoot 'target\debug\live-hub-rust.exe' }
}
if (-not $Fixture) { $Fixture = Join-Path $repoRoot 'tests\fixtures\overlay-scene-p0.json' }
if (-not $EvidenceDir) { $EvidenceDir = Join-Path $env:TEMP ('livehub-scene-' + [guid]::NewGuid().ToString('N').Substring(0,8)) }
if (-not (Test-Path -LiteralPath $Exe)) { throw ('overlay executable not found: ' + $Exe) }
if (-not (Test-Path -LiteralPath $Fixture)) { throw ('fixture not found: ' + $Fixture) }
New-Item -ItemType Directory -Path $EvidenceDir -Force | Out-Null
[void][SceneWin]::SetProcessDPIAware()

$temporary = Join-Path $env:TEMP ('livehub-scene-data-' + [guid]::NewGuid().ToString('N').Substring(0,8))
New-Item -ItemType Directory -Path $temporary -Force | Out-Null
if ($StateJson) {
    if (-not (Test-Path -LiteralPath $StateJson)) { throw ('state json not found: ' + $StateJson) }
    Copy-Item -LiteralPath $StateJson -Destination (Join-Path $temporary 'state.json')
}

$result = [ordered]@{
    kind = 'overlay-scene-render'
    generated_utc = [DateTime]::UtcNow.ToString('o')
    exe = $Exe
    fixture = $Fixture
    state_json = $StateJson
    sizes = $Sizes
    captures = @()
    foreground_before = $null
    foreground_after = $null
    error = $null
}
$overlayProcess = $null
$playerProcess = $null
try {
    $fg = [SceneWin]::GetForegroundWindow()
    $ownerPid = [uint32]0
    [void][SceneWin]::GetWindowThreadProcessId($fg, [ref]$ownerPid)
    $result.foreground_before = ('0x' + ([int64]$fg).ToString('X') + ':' + $ownerPid)
    $self = $PSCommandPath
    $playerProcess = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile','-STA','-ExecutionPolicy','Bypass','-File', $self, '-PlayerOnly','-PlayerTitle', $PlayerTitle) -PassThru -WindowStyle Hidden
    $playerHandle = [IntPtr]::Zero
    $deadline = [DateTime]::UtcNow.AddSeconds(40)
    while ([DateTime]::UtcNow -lt $deadline -and $playerHandle -eq [IntPtr]::Zero) {
        foreach ($w in [SceneWin]::WindowsOfProcess([uint32]$playerProcess.Id)) {
            if ([SceneWin]::WindowText($w) -eq $PlayerTitle) { $playerHandle = $w }
        }
        Start-Sleep -Milliseconds 100
    }
    if ($playerHandle -eq [IntPtr]::Zero) { throw 'synthetic player window was not created' }
    $rendererShot = Join-Path $EvidenceDir ($Prefix + '-renderer.png')
    if (Test-Path -LiteralPath $rendererShot) { Remove-Item -LiteralPath $rendererShot -Force }
    $envMap = @{
        LIVE_HUB_OVERLAY_FIXTURE = $Fixture
        LIVE_HUB_OVERLAY_DIAG_LOG = '1'
        LIVE_HUB_OVERLAY_DIAG_SCREENSHOT = $rendererShot
    }
    if (-not $NoForceShow) { $envMap['LIVE_HUB_OVERLAY_DIAG_FORCE_SHOW'] = '1' }
    if ($FreezeMs -gt 0) { $envMap['LIVE_HUB_OVERLAY_DIAG_FREEZE_MS'] = [string]$FreezeMs }
    $previous = @{}
    foreach ($key in $envMap.Keys) { $previous[$key] = [Environment]::GetEnvironmentVariable($key, 'Process') }
    foreach ($key in $envMap.Keys) { [Environment]::SetEnvironmentVariable($key, $envMap[$key], 'Process') }
    $arguments = @('--data-dir', $temporary, 'overlay', 'bili:7734200', '--player-pid', [string]$playerProcess.Id)
    try {
        $stderrPath = Join-Path $EvidenceDir ($Prefix + '-overlay-stderr.log')
        $stdoutPath = Join-Path $EvidenceDir ($Prefix + '-overlay-stdout.log')
        $overlayProcess = Start-Process -FilePath $Exe -ArgumentList $arguments -WorkingDirectory $repoRoot -PassThru -WindowStyle Hidden -RedirectStandardError $stderrPath -RedirectStandardOutput $stdoutPath
    } finally {
        foreach ($key in $envMap.Keys) { [Environment]::SetEnvironmentVariable($key, $previous[$key], 'Process') }
    }
    $overlayHandle = [IntPtr]::Zero
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while ([DateTime]::UtcNow -lt $deadline -and $overlayHandle -eq [IntPtr]::Zero) {
        foreach ($w in [SceneWin]::WindowsOfProcess([uint32]$overlayProcess.Id)) {
            if ([SceneWin]::WindowText($w) -like 'LiveHub Rust Overlay*') { $overlayHandle = $w }
        }
        Start-Sleep -Milliseconds 150
    }
    if ($overlayHandle -eq [IntPtr]::Zero) { throw 'overlay window was not created' }
    Start-Sleep -Milliseconds $WaitMs
    $index = 0
    foreach ($size in ($Sizes -split ',')) {
        $size = $size.Trim()
        if ($size.Length -eq 0) { continue }
        $parts = $size.Split('x')
        $width = [int]$parts[0]
        $height = [int]$parts[1]
        [void][SceneWin]::SetWindowPos($playerHandle, [IntPtr]::Zero, $X, $Y, $width, $height, 0x0014)
        Start-Sleep -Milliseconds $StepMs
        $index++
        $name = $Prefix + '-' + $size + '-' + $index
        $capture = Save-Capture $overlayHandle (Join-Path $EvidenceDir ($name + '.png'))
        $result.captures += $capture
        Write-Host ('captured ' + $capture.width + 'x' + $capture.height + ' -> ' + $capture.grey_png)
    }
    if ($PolicyCheck -and $overlayProcess -and -not $overlayProcess.HasExited) {
        $script:policySteps = @()
        $addStep = {
            param([string]$phase)
            $visible = [SceneWin]::IsWindowVisible($overlayHandle)
            $script:policySteps += [pscustomobject]@{
                phase = $phase
                overlay_visible = $visible
                player_minimized = [SceneWin]::IsIconic($playerHandle)
            }
        }
        & $addStep 'initial'
        [void][SceneWin]::ShowWindowAsync($playerHandle, 6)   # SW_MINIMIZE
        Start-Sleep -Milliseconds 1500
        & $addStep 'player_minimized'
        [void][SceneWin]::ShowWindowAsync($playerHandle, 9)   # SW_RESTORE
        Start-Sleep -Milliseconds 1500
        & $addStep 'player_restored'
        [void][SceneWin]::ShowWindowAsync($playerHandle, 0)   # SW_HIDE
        Start-Sleep -Milliseconds 1500
        & $addStep 'player_hidden'
        [void][SceneWin]::ShowWindowAsync($playerHandle, 4)   # SW_SHOWNOACTIVATE
        Start-Sleep -Milliseconds 1500
        & $addStep 'player_shown'
        $ex_style = [int64][SceneWin]::GetWindowLongPtrW($overlayHandle, -20)
        $result.policy = [pscustomobject]@{
            steps = $script:policySteps
            overlay_topmost = (($ex_style -band 0x8) -ne 0)
        }
    }
    $fg = [SceneWin]::GetForegroundWindow()
    $ownerPid = [uint32]0
    [void][SceneWin]::GetWindowThreadProcessId($fg, [ref]$ownerPid)
    if ($overlayProcess -and -not $overlayProcess.HasExited) { try { Stop-Process -Id $overlayProcess.Id -Force; $overlayProcess.WaitForExit(3000) } catch {} }
    if (Test-Path -LiteralPath $stderrPath) { $result.overlay_stderr_tail = (Get-Content -LiteralPath $stderrPath -Tail 40 | Out-String) }
    if (Test-Path -LiteralPath $rendererShot) { $result.renderer_screenshot = $rendererShot }

    $result.foreground_after = ('0x' + ([int64]$fg).ToString('X') + ':' + $ownerPid)
} catch {
    $result.error = ($_.Exception.Message -replace '\s+', ' ')
    $result.error_position = ($_.InvocationInfo.PositionMessage -replace '\s+', ' ')
    $result.error_line = $_.InvocationInfo.ScriptLineNumber
} finally {
    if ($overlayProcess -and -not $overlayProcess.HasExited) { try { Stop-Process -Id $overlayProcess.Id -Force } catch {} }
    if ($playerProcess -and -not $playerProcess.HasExited) { try { Stop-Process -Id $playerProcess.Id -Force } catch {} }
    Start-Sleep -Milliseconds 300
    Remove-Item -Recurse -Force -LiteralPath $temporary -ErrorAction SilentlyContinue
}
$outPath = Join-Path $EvidenceDir ('overlay-scene-' + $Prefix + '.json')
$result | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $outPath -Encoding UTF8
Write-Host ('evidence: ' + $outPath)
if ($result.error) { Write-Host ('ERROR: ' + $result.error); exit 1 }
exit 0