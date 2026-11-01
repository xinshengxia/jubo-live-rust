param(
    [string]$Exe = '',
    [string]$EvidenceDir = '',
    [string]$Fixture = '',
    [string]$Target = 'bili:7734200',
    [switch]$PlayerOnly,
    [int]$X = -6000,
    [int]$Y = 100,
    [int]$Width = 960,
    [int]$Height = 540,
    [string]$PlayerTitle = 'LiveHubAcceptancePlayer'
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$user32 = @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

public static class AccWin
{
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X; public int Y; }
    [StructLayout(LayoutKind.Sequential)] public struct MSG {
        public IntPtr hwnd; public uint message; public IntPtr wParam; public IntPtr lParam; public uint time; public POINT pt; public uint lPrivate;
    }
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);

    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)] public static extern IntPtr CreateWindowExW(uint dwExStyle, string lpClassName, string lpWindowName, uint dwStyle, int x, int y, int nWidth, int nHeight, IntPtr hWndParent, IntPtr hMenu, IntPtr hInstance, IntPtr lpParam);
    [DllImport("user32.dll", SetLastError = true)] public static extern bool DestroyWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr hWnd);
    [DllImport("user32.dll", SetLastError = true)] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int X, int Y, int cx, int cy, uint uFlags);
    [DllImport("user32.dll", SetLastError = true)] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll", SetLastError = true)] public static extern bool GetClientRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll", SetLastError = true)] public static extern bool ClientToScreen(IntPtr hWnd, ref POINT lpPoint);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll", SetLastError = true)] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr hWnd, StringBuilder lpClassName, int nMaxCount);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr hWnd, StringBuilder lpString, int nMaxCount);
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr hWnd, uint uCmd);
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT point);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdcBlt, uint nFlags);
    [DllImport("user32.dll")] public static extern IntPtr GetWindowLongPtrW(IntPtr hWnd, int nIndex);
    [DllImport("user32.dll")] public static extern IntPtr SetWindowLongPtrW(IntPtr hWnd, int nIndex, IntPtr dwNewLong);
    [DllImport("user32.dll")] public static extern bool PeekMessageW(out MSG lpMsg, IntPtr hWnd, uint wMsgFilterMin, uint wMsgFilterMax, uint wRemoveMsg);
    [DllImport("user32.dll")] public static extern bool TranslateMessage(ref MSG lpMsg);
    [DllImport("user32.dll")] public static extern IntPtr DispatchMessageW(ref MSG lpMsg);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int nIndex);
    [DllImport("user32.dll")] public static extern bool SetLayeredWindowAttributes(IntPtr hWnd, uint crKey, byte bAlpha, uint dwFlags);
    [DllImport("user32.dll")] public static extern IntPtr GetDesktopWindow();
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc lpEnumFunc, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();

    public static IntPtr[] WindowsOfProcess(uint pid)
    {
        List<IntPtr> found = new List<IntPtr>();
        EnumWindows(delegate(IntPtr h, IntPtr l)
        {
            uint owner = 0;
            GetWindowThreadProcessId(h, out owner);
            if (owner == pid) { found.Add(h); }
            return true;
        }, IntPtr.Zero);
        return found.ToArray();
    }

    public static string ClassName(IntPtr h) { StringBuilder sb = new StringBuilder(256); GetClassNameW(h, sb, sb.Capacity); return sb.ToString(); }
    public static string WindowText(IntPtr h) { StringBuilder sb = new StringBuilder(1024); GetWindowTextW(h, sb, sb.Capacity); return sb.ToString(); }
    public static RECT WindowRectangle(IntPtr h) { RECT r; GetWindowRect(h, out r); return r; }
    public static RECT ClientRectangle(IntPtr h) { RECT r; GetClientRect(h, out r); return r; }
    public static POINT ClientOrigin(IntPtr h) { POINT p; p.X = 0; p.Y = 0; ClientToScreen(h, ref p); return p; }
    public static IntPtr HitTest(int x, int y) { POINT p; p.X = x; p.Y = y; return WindowFromPoint(p); }

    public static void Pump(int milliseconds)
    {
        int end = Environment.TickCount + milliseconds;
        MSG message;
        while (Environment.TickCount < end)
        {
            while (PeekMessageW(out message, IntPtr.Zero, 0, 0, 1)) { TranslateMessage(ref message); DispatchMessageW(ref message); }
            System.Threading.Thread.Sleep(10);
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

if ($PlayerOnly) {
    [void][AccWin]::SetProcessDPIAware()
    $playerHandle = [AccWin]::CreatePlayer($PlayerTitle, $X, $Y, $Width, $Height)
    while ([AccWin]::IsWindow($playerHandle)) { [AccWin]::Pump(200) }
    exit 0
}

# Defaults resolve from the repository so the harness can run from a clean checkout:
# the built debug launcher, the committed synthetic fixture and a TEMP evidence dir.
$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $Exe) {
    if ($env:LIVE_HUB_ACCEPTANCE_OVERLAY_EXE) { $Exe = $env:LIVE_HUB_ACCEPTANCE_OVERLAY_EXE }
    else { $Exe = Join-Path $repoRoot 'target\debug\live-hub-rust.exe' }
}
if (-not $EvidenceDir) { $EvidenceDir = Join-Path $env:TEMP 'livehub-overlay-evidence' }
if (-not $Fixture) { $Fixture = Join-Path $repoRoot 'tests\fixtures\overlay_diagnostic.json' }
New-Item -ItemType Directory -Path $EvidenceDir -Force | Out-Null
if (-not (Test-Path -LiteralPath $Exe)) { throw ('overlay executable not found: ' + $Exe) }
if (-not (Test-Path -LiteralPath $Fixture)) { throw ('overlay fixture not found: ' + $Fixture) }

# Physical-pixel coordinates keep GetWindowRect/ClientToScreen/WindowFromPoint
# consistent with the DPI-aware overlay process; record the resulting screen size
# and Windows DPI for the evidence (no system DPI change is made).
[void][AccWin]::SetProcessDPIAware()
$dpiAware = $true

$pixelStats = @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;

public static class PixelStats
{
    public static string Analyze(Bitmap bitmap)
    {
        int width = bitmap.Width;
        int height = bitmap.Height;
        BitmapData data = bitmap.LockBits(new Rectangle(0, 0, width, height), ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
        byte[] bytes = new byte[data.Stride * height];
        Marshal.Copy(data.Scan0, bytes, 0, bytes.Length);
        bitmap.UnlockBits(data);
        long alphaNonZero = 0;
        long rgbNonZero = 0;
        long bright = 0;
        long colored = 0;
        int minX = int.MaxValue, minY = int.MaxValue, maxX = -1, maxY = -1;
        for (int y = 0; y < height; y++)
        {
            int row = y * data.Stride;
            for (int x = 0; x < width; x++)
            {
                int offset = row + x * 4;
                byte b = bytes[offset];
                byte g = bytes[offset + 1];
                byte r = bytes[offset + 2];
                byte a = bytes[offset + 3];
                if (a != 0) { alphaNonZero++; }
                if ((r | g | b) != 0)
                {
                    rgbNonZero++;
                    if (x < minX) { minX = x; }
                    if (y < minY) { minY = y; }
                    if (x > maxX) { maxX = x; }
                    if (y > maxY) { maxY = y; }
                }
                int max = Math.Max(r, Math.Max(g, b));
                int min = Math.Min(r, Math.Min(g, b));
                if (max > 180 && a > 100) { bright++; }
                if (max - min > 60 && max > 120) { colored++; }
            }
        }
        return String.Format("width={0};height={1};alpha_nonzero={2};rgb_nonzero={3};bright={4};colored={5};bbox={6},{7},{8},{9}", width, height, alphaNonZero, rgbNonZero, bright, colored, minX, minY, maxX, maxY);
    }
}
'@
Add-Type -TypeDefinition $pixelStats -Language CSharp -ReferencedAssemblies 'System.Drawing'
Add-Type -AssemblyName System.Drawing

$script:baseline = $null
$script:overlayHandle = [IntPtr]::Zero
$script:playerHandle = [IntPtr]::Zero
$script:playerProcess = $null
$script:overlayProcess = $null
$steps = New-Object System.Collections.ArrayList
function Add-Step([string]$name, $value) {
    $entry = [pscustomobject]@{ step = $name; value = $value }
    [void]$steps.Add($entry)
    Write-Host ("STEP " + $name + ": " + (($value | ConvertTo-Json -Compress -Depth 8)))
    return $value
}

function Get-ForegroundInfo {
    $handle = [AccWin]::GetForegroundWindow()
    $owner = [uint32]0
    [void][AccWin]::GetWindowThreadProcessId($handle, [ref]$owner)
    $text = [AccWin]::ClassName($handle)
    return [pscustomobject]@{ hwnd = ('0x' + ([int64]$handle).ToString('X')); pid = $owner; class = $text }
}

function Get-WindowInfo([IntPtr]$handle) {
    if ($handle -eq [IntPtr]::Zero) { return $null }
    $rect = [AccWin]::WindowRectangle($handle)
    $client = [AccWin]::ClientRectangle($handle)
    $origin = [AccWin]::ClientOrigin($handle)
    $owner = [uint32]0
    [void][AccWin]::GetWindowThreadProcessId($handle, [ref]$owner)
    $previous = [int64][AccWin]::GetWindow($handle, 3)
    return [pscustomobject]([ordered]@{
        hwnd = ('0x' + ([int64]$handle).ToString('X'))
        class = [AccWin]::ClassName($handle)
        title = [AccWin]::WindowText($handle)
        pid = $owner
        visible = [AccWin]::IsWindowVisible($handle)
        iconic = [AccWin]::IsIconic($handle)
        window_rect = @($rect.Left, $rect.Top, $rect.Right, $rect.Bottom)
        client_size = @(($client.Right - $client.Left), ($client.Bottom - $client.Top))
        client_origin = @($origin.X, $origin.Y)
        style = ('0x' + ([int64][AccWin]::GetWindowLongPtrW($handle, -16)).ToString('X'))
        exstyle = ('0x' + ([int64][AccWin]::GetWindowLongPtrW($handle, -20)).ToString('X'))
        hwnd_above = ('0x' + $previous.ToString('X'))
    })
}

function Get-Geometry([IntPtr]$overlay, [IntPtr]$player) {
    $origin = [AccWin]::ClientOrigin($player)
    $client = [AccWin]::ClientRectangle($player)
    $rect = [AccWin]::WindowRectangle($overlay)
    $width = $client.Right - $client.Left
    $height = $client.Bottom - $client.Top
    $deltaX = $rect.Left - $origin.X
    $deltaY = $rect.Top - $origin.Y
    $deltaW = ($rect.Right - $rect.Left) - $width
    $deltaH = ($rect.Bottom - $rect.Top) - $height
    return [pscustomobject]([ordered]@{
        player_client_origin = @($origin.X, $origin.Y)
        player_client_size = @($width, $height)
        overlay_window_rect = @($rect.Left, $rect.Top, $rect.Right, $rect.Bottom)
        delta_origin = @($deltaX, $deltaY)
        delta_size = @($deltaW, $deltaH)
        matched = (([math]::Abs($deltaX) -le 2) -and ([math]::Abs($deltaY) -le 2) -and ([math]::Abs($deltaW) -le 2) -and ([math]::Abs($deltaH) -le 2))
    })
}

function Wait-Until([scriptblock]$condition, [int]$timeoutMs) {
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (& $condition) { return $true }
        Start-Sleep -Milliseconds 50
    }
    return $false
}

function Start-HiddenProcess([string]$fileName, [string]$arguments, [hashtable]$environment) {
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $fileName
    $info.Arguments = $arguments
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    if ($environment) { foreach ($key in $environment.Keys) { $info.EnvironmentVariables[$key] = [string]$environment[$key] } }
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = $info
    [void]$process.Start()
    return $process
}

function Save-WindowCapture([IntPtr]$handle, [string]$path, [string]$rgbPath) {
    $rect = [AccWin]::WindowRectangle($handle)
    $width = [math]::Max(1, $rect.Right - $rect.Left)
    $height = [math]::Max(1, $rect.Bottom - $rect.Top)
    $bitmap = New-Object -TypeName System.Drawing.Bitmap -ArgumentList @($width, $height, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $deviceContext = $graphics.GetHdc()
    $printed = [AccWin]::PrintWindow($handle, $deviceContext, 2)
    $graphics.ReleaseHdc($deviceContext)
    $graphics.Dispose()
    $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $stats = [PixelStats]::Analyze($bitmap)
    $rgb = New-Object -TypeName System.Drawing.Bitmap -ArgumentList @($width, $height, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $rgbGraphics = [System.Drawing.Graphics]::FromImage($rgb)
    $rgbGraphics.Clear([System.Drawing.Color]::Black)
    $rgbGraphics.DrawImage($bitmap, 0, 0, $width, $height)
    $rgbGraphics.Dispose()
    $rgb.Save($rgbPath, [System.Drawing.Imaging.ImageFormat]::Png)
    $bitmap.Dispose()
    $rgb.Dispose()
    return [pscustomobject]@{ printwindow_ok = $printed; width = $width; height = $height; stats = $stats; png = $path; rgb_png = $rgbPath }
}

function Save-ScreenCapture([int]$x, [int]$y, [int]$width, [int]$height, [string]$path) {
    $bitmap = New-Object -TypeName System.Drawing.Bitmap -ArgumentList @($width, $height, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($x, $y, 0, 0, (New-Object System.Drawing.Size($width, $height)))
    $graphics.Dispose()
    $bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $stats = [PixelStats]::Analyze($bitmap)
    $bitmap.Dispose()
    return [pscustomobject]@{ width = $width; height = $height; stats = $stats; png = $path }
}

$temporary = Join-Path $env:TEMP ('livehub-overlay-' + [guid]::NewGuid().ToString('N').Substring(0,8))
New-Item -ItemType Directory -Path $temporary -Force | Out-Null
$overlayStartedAt = $null
$result = [ordered]@{
    kind = 'overlay-window-acceptance'
    generated_utc = [DateTime]::UtcNow.ToString('o')
    exe = $Exe
    fixture = $Fixture
    target = $Target
    steps = $steps
    foreground = [ordered]@{}
    styles = $null
    geometry = (New-Object System.Collections.ArrayList)
    minimize = $null
    hide = $null
    click_through = $null
    z_order = $null
    capture = $null
    close = $null
    cleanup = $null
    error = $null
}
try {
    $baselineForeground = Get-ForegroundInfo
    $result.foreground.baseline = $baselineForeground

    $playerProcess = Start-HiddenProcess -fileName 'powershell.exe' -arguments ('-NoProfile -STA -ExecutionPolicy Bypass -File "' + $PSCommandPath + '" -PlayerOnly -PlayerTitle "' + $PlayerTitle + '"') -environment $null
    $playerHandle = [IntPtr]::Zero
    $found = Wait-Until { foreach ($window in [AccWin]::WindowsOfProcess([uint32]$playerProcess.Id)) { if ([AccWin]::ClassName($window) -eq 'STATIC') { $script:playerHandle = $window; return $true } }; return $false } 15000
    if (-not $found) { throw 'synthetic player window was not created' }
    $result.foreground.after_player = Get-ForegroundInfo
    Add-Step 'player_window' (Get-WindowInfo $playerHandle)

    $environment = @{
        LIVE_HUB_OVERLAY_DIAG_FORCE_SHOW = '1'
        LIVE_HUB_OVERLAY_FIXTURE = $Fixture
        LIVE_HUB_OVERLAY_DIAG_LOG = '1'
    }
    $overlayArguments = '--data-dir "' + (Join-Path $temporary 'data') + '" overlay ' + $Target + ' --player-pid ' + $playerProcess.Id
    $overlayStartedAt = [DateTime]::UtcNow
    $overlayProcess = Start-HiddenProcess -fileName $Exe -arguments $overlayArguments -environment $environment

    $found = Wait-Until { foreach ($window in [AccWin]::WindowsOfProcess([uint32]$overlayProcess.Id)) { if ([AccWin]::WindowText($window) -like 'LiveHub Rust Overlay*') { $script:overlayHandle = $window; return $true } }; return $false } 30000
    if (-not $found) { throw 'overlay window was not created' }
    $visible = Wait-Until { [AccWin]::IsWindowVisible($script:overlayHandle) } 10000
    Add-Step 'overlay_visible_after_launch' $visible
    if (-not $visible) { throw 'overlay window did not become visible' }
    Start-Sleep -Milliseconds 700
    $result.foreground.after_overlay = Get-ForegroundInfo
    $windowInfo = Get-WindowInfo $overlayHandle
    $result.styles = $windowInfo
    Add-Step 'overlay_window' $windowInfo

    $geometry = Get-Geometry $overlayHandle $playerHandle
    [void]$result.geometry.Add([pscustomobject]@{ phase = 'initial'; data = $geometry })
    $result.foreground.after_initial_geometry = Get-ForegroundInfo

    $moves = @(
        @{ x = -6100; y = 150; width = 800; height = 450 },
        @{ x = -5800; y = 120; width = 1280; height = 720 }
    )
    foreach ($move in $moves) {
        [void][AccWin]::SetWindowPos($playerHandle, [IntPtr]::Zero, $move.x, $move.y, $move.width, $move.height, 0x0014)
        $moved = Wait-Until { (Get-Geometry $script:overlayHandle $script:playerHandle).matched } 3000
        $snapshot = Get-Geometry $overlayHandle $playerHandle
        [void]$result.geometry.Add([pscustomobject]@{ phase = ('move_to_' + $move.x + '_' + $move.width); followed = $moved; data = $snapshot })
        Add-Step ('follow_' + $move.width) $snapshot
    }

    [void][AccWin]::ShowWindowAsync($playerHandle, 6)
    $iconic = Wait-Until { [AccWin]::IsIconic($script:playerHandle) } 3000
    $overlayHidden = Wait-Until { -not [AccWin]::IsWindowVisible($script:overlayHandle) } 4000
    $result.minimize = [pscustomobject]@{ iconic = $iconic; overlay_hidden = $overlayHidden; overlay_visible = [AccWin]::IsWindowVisible($overlayHandle); restored_overlay_visible = $null }
    Add-Step 'minimize' $result.minimize
    [void][AccWin]::ShowWindowAsync($playerHandle, 4)
    $restored = Wait-Until { (-not [AccWin]::IsIconic($script:playerHandle)) -and [AccWin]::IsWindowVisible($script:overlayHandle) } 5000
    $result.minimize = [pscustomobject]@{ iconic = $iconic; overlay_hidden = $overlayHidden; overlay_visible = $false; restored_overlay_visible = $restored }
    Add-Step 'restore' ([pscustomobject]@{ restored = $restored })

    [void][AccWin]::ShowWindowAsync($playerHandle, 0)
    $playerHidden = Wait-Until { -not [AccWin]::IsWindowVisible($script:playerHandle) } 3000
    $hideOverlayHidden = Wait-Until { -not [AccWin]::IsWindowVisible($script:overlayHandle) } 4000
    $result.hide = [pscustomobject]@{ player_hidden = $playerHidden; overlay_hidden = $hideOverlayHidden; reshown_overlay_visible = $null }
    Add-Step 'hide_player' $result.hide
    [void][AccWin]::ShowWindowAsync($playerHandle, 4)
    $reshown = Wait-Until { [AccWin]::IsWindowVisible($script:playerHandle) -and [AccWin]::IsWindowVisible($script:overlayHandle) } 5000
    $result.hide = [pscustomobject]@{ player_hidden = $playerHidden; overlay_hidden = $hideOverlayHidden; reshown_overlay_visible = $reshown }
    Add-Step 'show_player' ([pscustomobject]@{ overlay_visible = $reshown })

    $waitForFixture = 15000 - ([DateTime]::UtcNow - $overlayStartedAt).TotalMilliseconds
    if ($waitForFixture -gt 0) { Start-Sleep -Milliseconds ([int]$waitForFixture) }
    $overlayRect = [AccWin]::WindowRectangle($overlayHandle)
    $capture = Save-WindowCapture $overlayHandle (Join-Path $EvidenceDir 'overlay_printwindow.png') (Join-Path $EvidenceDir 'overlay_printwindow_rgb.png')
    $result.capture = $capture
    Add-Step 'printwindow_capture' ([pscustomobject]@{ printwindow_ok = $capture.printwindow_ok; stats = $capture.stats })

    # Click-through acceptance: move an invisible (WS_EX_LAYERED, alpha 0)
    # synthetic player onto the screen centre, then A/B hit-test with a 1x1
    # reference window as positive control. No focus is taken, no input is sent.
    $screenWidth = [AccWin]::GetSystemMetrics(0)
    $screenHeight = [AccWin]::GetSystemMetrics(1)
    $pointX = [int]($screenWidth / 2)
    $pointY = [int]($screenHeight / 2)
    $playerWidth = 640
    $playerHeight = 360
    $targetX = [int][math]::Max(0, [math]::Min($screenWidth - $playerWidth, $pointX - [int]($playerWidth / 2)))
    $targetY = [int][math]::Max(0, [math]::Min($screenHeight - $playerHeight, $pointY - [int]($playerHeight / 2)))
    $playerEx = [int64][AccWin]::GetWindowLongPtrW($playerHandle, -20)
    [void][AccWin]::SetWindowLongPtrW($playerHandle, -20, [IntPtr]($playerEx -bor 0x80000))
    $alphaSet = [AccWin]::SetLayeredWindowAttributes($playerHandle, 0, 0, 2)
    # Raise the player above other normal windows (without activation) so the
    # follow watcher places the overlay directly above it; otherwise Chrome would
    # cover both and WindowFromPoint could not observe the overlay at all.
    [void][AccWin]::SetWindowPos($playerHandle, [IntPtr]::Zero, $targetX, $targetY, $playerWidth, $playerHeight, 0x0010)
    $followed = Wait-Until { (Get-Geometry $script:overlayHandle $script:playerHandle).matched } 3000
    $overlayRect = [AccWin]::WindowRectangle($overlayHandle)
    $centerX = [int](($overlayRect.Left + $overlayRect.Right) / 2)
    $centerY = [int](($overlayRect.Top + $overlayRect.Bottom) / 2)
    $onscreenCapture = Save-ScreenCapture $overlayRect.Left $overlayRect.Top ($overlayRect.Right - $overlayRect.Left) ($overlayRect.Bottom - $overlayRect.Top) (Join-Path $EvidenceDir 'overlay_onscreen_capture.png')
    $clickThrough = [ordered]@{
        point = @($centerX, $centerY)
        screen = @($screenWidth, $screenHeight)
        player_layer_alpha_set = $alphaSet
        followed_onscreen = $followed
        onscreen_capture = $onscreenCapture
        positive_control = $null
        hit = $null
        note = $null
        offscreen_again = $null
    }
    # Positive control: a 1x1 normal window raised to the top proves the point is
    # hit-testable and that WindowFromPoint really inspects this screen area.
    $referenceHandle = [AccWin]::CreateWindowExW(0x80, 'STATIC', 'LiveHubHitReference', ([uint32]2415919104), $centerX, $centerY, 1, 1, [IntPtr]::Zero, [IntPtr]::Zero, [IntPtr]::Zero, [IntPtr]::Zero)
    if ($referenceHandle -ne [IntPtr]::Zero) {
        [void][AccWin]::SetWindowPos($referenceHandle, [IntPtr]::Zero, $centerX, $centerY, 1, 1, 0x0010)
        [void][AccWin]::ShowWindowAsync($referenceHandle, 4)
        Start-Sleep -Milliseconds 300
        $controlHit = [AccWin]::HitTest($centerX, $centerY)
        $clickThrough.reference_window = ('0x' + ([int64]$referenceHandle).ToString('X'))
        $clickThrough.positive_control = [pscustomobject]@{ hwnd = ('0x' + ([int64]$controlHit).ToString('X')); class = [AccWin]::ClassName($controlHit); is_reference = ($controlHit -eq $referenceHandle) }
        [void][AccWin]::DestroyWindow($referenceHandle)
        Start-Sleep -Milliseconds 300
    } else {
        $clickThrough.note = '1x1 reference window could not be created'
    }
    $hit = [AccWin]::HitTest($centerX, $centerY)
    $clickThrough.hit = [pscustomobject]@{
        hwnd = ('0x' + ([int64]$hit).ToString('X'))
        class = if ($hit -eq [IntPtr]::Zero) { '' } else { [AccWin]::ClassName($hit) }
        is_overlay = ($hit -eq $overlayHandle)
        is_player = ($hit -eq $playerHandle)
    }
    [void][AccWin]::SetWindowPos($playerHandle, [IntPtr]::Zero, -6100, 150, 800, 450, 0x0014)
    Start-Sleep -Milliseconds 500
    $clickThrough.offscreen_again = (Get-Geometry $overlayHandle $playerHandle)
    $result.click_through = $clickThrough
    Add-Step 'click_through' $clickThrough

    $aboveHandle = [AccWin]::GetWindow($playerHandle, 3)
    $overlayZ = [ordered]@{
        window_above_player = ('0x' + ([int64]$aboveHandle).ToString('X'))
        overlay_is_above_player = ($aboveHandle -eq $overlayHandle)
        overlay_topmost = (([int64][AccWin]::GetWindowLongPtrW($overlayHandle, -20) -band 0x8) -ne 0)
    }
    $result.z_order = $overlayZ
    Add-Step 'z_order' $overlayZ
    $result.foreground.after_steps = Get-ForegroundInfo

    $playerExit = $false
    $closeStarted = [DateTime]::UtcNow
    Stop-Process -Id $playerProcess.Id -Force
    $playerExit = $playerProcess.WaitForExit(3000)
    $overlayExited = Wait-Until { $script:overlayProcess.HasExited } 25000
    $result.close = [pscustomobject]@{
        player_pid = $playerProcess.Id
        overlay_pid = $overlayProcess.Id
        player_process_exited = $playerExit
        overlay_exited_after_player_close = $overlayExited
        overlay_exit_elapsed_ms = [int]([DateTime]::UtcNow - $closeStarted).TotalMilliseconds
        overlay_exit_code = $(if ($overlayExited) { $overlayProcess.ExitCode } else { $null })
        remaining_overlay_windows = @([AccWin]::WindowsOfProcess([uint32]$overlayProcess.Id) | ForEach-Object { '0x' + ([int64]$_).ToString('X') })
    }
    Add-Step 'close_player' $result.close
    $result.foreground.final = Get-ForegroundInfo
}
catch {
    $result.error = ($_.Exception.Message -replace '\s+', ' ')
} finally {
    if ($overlayProcess -and -not $overlayProcess.HasExited) { try { Stop-Process -Id $overlayProcess.Id -Force } catch {} }
    if ($playerProcess -and -not $playerProcess.HasExited) { try { Stop-Process -Id $playerProcess.Id -Force } catch {} }
    $result.cleanup = [pscustomobject]@{
        overlay_alive = $(if ($overlayProcess) { -not $overlayProcess.HasExited } else { $false })
        player_alive = $(if ($playerProcess) { -not $playerProcess.HasExited } else { $false })
        remaining_player_windows = @()
        remaining_overlay_windows = @()
    }
    if ($playerProcess) { $result.cleanup.remaining_player_windows = @([AccWin]::WindowsOfProcess([uint32]$playerProcess.Id) | ForEach-Object { '0x' + ([int64]$_).ToString('X') }) }
    if ($overlayProcess) { $result.cleanup.remaining_overlay_windows = @([AccWin]::WindowsOfProcess([uint32]$overlayProcess.Id) | ForEach-Object { '0x' + ([int64]$_).ToString('X') }) }
    if ($overlayProcess) { try { $stderrText = $overlayProcess.StandardError.ReadToEnd(); $stdoutText = $overlayProcess.StandardOutput.ReadToEnd(); $result.overlay_stdout = $stdoutText; $result.overlay_stderr = $stderrText } catch {} }
    Remove-Item -Recurse -Force -LiteralPath $temporary -ErrorAction SilentlyContinue
}
$outPath = Join-Path $EvidenceDir 'overlay_acceptance.json'
$result | ConvertTo-Json -Depth 14 | Set-Content -LiteralPath $outPath -Encoding UTF8
Write-Host ('evidence: ' + $outPath)
if ($result.error) { Write-Host ('ERROR: ' + $result.error); exit 1 }
exit 0
