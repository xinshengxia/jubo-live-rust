[CmdletBinding()]
param(
    [ValidateSet('synthetic-smoke', 'synthetic-soak', 'synthetic-all', 'real-soak')]
    [string]$Mode = 'synthetic-smoke',
    [string]$ToolchainRoot = $env:LIVE_HUB_RUST_TOOLCHAIN,
    # Must be a pure-ASCII path: the GNU toolchain cannot link from a Unicode target directory.
    [string]$TargetDirectory,
    [string]$EvidenceDirectory,
    [int]$StormSeconds = 60,
    [int]$SoakSeconds = 1500,
    [int]$SampleMilliseconds = 30000,
    [int]$Players = 8,
    [string]$RealTarget = 'huya:660000',
    [int]$RealPlayers = 3,
    [int]$RealSeconds = 600
)
$ErrorActionPreference = 'Stop'
if (-not $ToolchainRoot) { throw 'Provide -ToolchainRoot (isolated Rust toolchain root).' }
if (-not $TargetDirectory) { throw 'Provide -TargetDirectory (pure-ASCII target directory).' }
if (-not $EvidenceDirectory) { throw 'Provide -EvidenceDirectory for logs and metric tables.' }
$repository = Split-Path -Parent $PSScriptRoot
$cargoRunner = Join-Path $PSScriptRoot 'Invoke-Cargo.ps1'
New-Item -ItemType Directory -Force -Path $EvidenceDirectory | Out-Null
$started = Get-Date
$summary = [ordered]@{
    mode = $Mode
    started = $started.ToString('o')
    repository = $repository
    evidence = (Resolve-Path -LiteralPath $EvidenceDirectory).Path
    steps = @()
}
function Add-Step([string]$Name, [string]$Status, [hashtable]$Detail = @{}) {
    $script:summary.steps += [ordered]@{ name = $Name; status = $Status; detail = $Detail }
}
function Invoke-Cargo([string[]]$Arguments) {
    & $cargoRunner -ToolchainRoot $ToolchainRoot -TargetDirectory $TargetDirectory -CargoArguments $Arguments
    if ($LASTEXITCODE -ne 0) { throw "cargo exited with $LASTEXITCODE" }
}
function Get-StressExecutable {
    $candidate = Get-ChildItem -LiteralPath (Join-Path $TargetDirectory 'debug/deps') -Filter 'relay_stress-*.exe' |
        Sort-Object LastWriteTime | Select-Object -Last 1
    if (-not $candidate) { throw 'relay_stress test binary is missing; build it first.' }
    $candidate.FullName
}
function Invoke-StressTest([string]$Name, [hashtable]$Environment, [string]$LogName) {
    $executable = Get-StressExecutable
    $saved = @{}
    foreach ($key in $Environment.Keys) {
        $saved[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
        [Environment]::SetEnvironmentVariable($key, [string]$Environment[$key], 'Process')
    }
    try {
        $log = Join-Path $EvidenceDirectory $LogName
        Write-Host "== $Name -> $log"
        & $executable --ignored --nocapture --test-threads=1 $Name 2>&1 | Tee-Object -FilePath $log
        if ($LASTEXITCODE -ne 0) { throw "stress test $Name failed with exit code $LASTEXITCODE (log: $log)" }
    } finally {
        foreach ($key in $saved.Keys) { [Environment]::SetEnvironmentVariable($key, $saved[$key], 'Process') }
    }
}
function Get-ForegroundWindow {
    if (-not ('LiveHubStressNative' -as [type])) {
        Add-Type -Namespace LiveHubStressNative -Name Win32 -MemberDefinition @'
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
'@
    }
    [LiveHubStressNative.Win32]::GetForegroundWindow()
}
function Invoke-Synthetic {
    Invoke-Cargo @('test', '--locked', '--offline', '--no-default-features', '--test', 'relay_stress', '--no-run')
    Add-Step 'build' 'ok'
    $foregroundBefore = Get-ForegroundWindow
    if ($Mode -in @('synthetic-smoke', 'synthetic-all')) {
        Invoke-StressTest 'stress_rejects_bad_tokens_and_ninth_idle_connection' @{} 'storm-token-rejection.log'
        Add-Step 'token-and-budget' 'ok'
        Invoke-StressTest 'stress_eight_players_survive_reconnect_storm' @{
            STRESS_STORM_SECS = $StormSeconds
            STRESS_EVIDENCE = (Join-Path $EvidenceDirectory 'storm.json')
        } 'storm-8-players.log'
        Add-Step 'reconnect-storm' 'ok' @{ seconds = $StormSeconds; players = 8 }
    }
    if ($Mode -in @('synthetic-soak', 'synthetic-all')) {
        Invoke-StressTest 'stress_soak_supervision_and_resources' @{
            STRESS_SOAK_SECS = $SoakSeconds
            STRESS_SAMPLE_MS = $SampleMilliseconds
            STRESS_PLAYERS = $Players
            STRESS_EVIDENCE = (Join-Path $EvidenceDirectory 'soak.json')
        } 'soak.log'
        Add-Step 'soak' 'ok' @{ seconds = $SoakSeconds; players = $Players; sample_ms = $SampleMilliseconds }
    }
    $foregroundAfter = Get-ForegroundWindow
    $script:summary.foreground_before = $foregroundBefore.ToString()
    $script:summary.foreground_after = $foregroundAfter.ToString()
    Add-Step 'foreground' ($(if ($foregroundBefore -eq $foregroundAfter) { 'unchanged' } else { 'changed' }))
}
function Invoke-RealSoak {
    Invoke-Cargo @('build', '--locked', '--offline', '--no-default-features')
    $binary = Join-Path $TargetDirectory 'debug/live-hub-rust.exe'
    if (-not (Test-Path -LiteralPath $binary)) { throw "relay binary is missing: $binary" }
    Add-Step 'build' 'ok'
    $temporaryRoot = Join-Path $env:TEMP ('livehub-relay-real-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $temporaryRoot | Out-Null
    $token = 'soak' + [Guid]::NewGuid().ToString('N')
    $foregroundBefore = Get-ForegroundWindow
    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $binary
    $startInfo.Arguments = '--data-dir "' + $temporaryRoot + '" relay ' + $RealTarget + ' --token ' + $token + ' --port 0'
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.CreateNoWindow = $true
    $relay = [System.Diagnostics.Process]::Start($startInfo)
    try {
        $readyTask = $relay.StandardOutput.ReadLineAsync()
        if (-not $readyTask.Wait(30000)) { throw 'relay did not print READY within 30s' }
        $line = $readyTask.Result
        if (-not $line -or -not $line.StartsWith('READY ')) { throw "unexpected relay stdout: $line" }
        $port = [int]$line.Substring(6).Trim()
        $url = "http://127.0.0.1:$port/live.flv?t=$token"
        Write-Host "relay ready on port $port for $RealTarget"
        $jobs = @()
        for ($index = 1; $index -le $RealPlayers; $index++) {
            $jobs += Start-Job -ScriptBlock {
                param($Address, $Request, $Seconds)
                $client = [System.Net.Sockets.TcpClient]::new()
                try {
                    $client.Connect($Address.Split(':')[0], [int]$Address.Split(':')[1])
                    $stream = $client.GetStream()
                    $bytes = [Text.Encoding]::ASCII.GetBytes($Request)
                    $stream.Write($bytes, 0, $bytes.Length)
                    $buffer = New-Object byte[] 16384
                    $total = 0L
                    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
                    while ([DateTime]::UtcNow -lt $deadline) {
                        if (-not $stream.DataAvailable) { Start-Sleep -Milliseconds 20; continue }
                        $read = $stream.Read($buffer, 0, $buffer.Length)
                        if ($read -le 0) { break }
                        $total += $read
                    }
                    [pscustomobject]@{ bytes = $total; error = $null }
                } catch {
                    [pscustomobject]@{ bytes = 0; error = $_.Exception.Message }
                } finally { $client.Dispose() }
            } -ArgumentList ("127.0.0.1:" + $port), ("GET /live.flv?t=" + $token + " HTTP/1.1
Host: localhost

"), $RealSeconds
        }
        $samples = @()
        $deadline = (Get-Date).AddSeconds($RealSeconds)
        $nextSample = Get-Date
        while ((Get-Date) -lt $deadline) {
            if ($relay.HasExited) { throw "relay exited early with code $($relay.ExitCode)" }
            if ((Get-Date) -ge $nextSample) {
                $relay.Refresh()
                $sample = [ordered]@{
                    elapsed_s = [math]::Round(((Get-Date) - $started).TotalSeconds, 1)
                    rss_mb = [math]::Round($relay.WorkingSet64 / 1MB, 1)
                    private_mb = [math]::Round($relay.PrivateMemorySize64 / 1MB, 1)
                    handles = $relay.HandleCount
                    threads = $relay.Threads.Count
                    cpu_s = [math]::Round($relay.TotalProcessorTime.TotalSeconds, 2)
                }
                $samples += $sample
                Write-Host ("sample t={0}s rss={1}MiB private={2}MiB handles={3} threads={4}" -f $sample.elapsed_s, $sample.rss_mb, $sample.private_mb, $sample.handles, $sample.threads)
                $nextSample = (Get-Date).AddMilliseconds($SampleMilliseconds)
            }
            Start-Sleep -Milliseconds 200
        }
        Wait-Job -Job $jobs -Timeout ($RealSeconds + 60) | Out-Null
        $results = foreach ($job in $jobs) {
            $output = Receive-Job -Job $job -ErrorAction SilentlyContinue
            Stop-Job -Job $job -ErrorAction SilentlyContinue
            Remove-Job -Job $job -Force -ErrorAction SilentlyContinue
            $output
        }
        $totalBytes = ($results | Measure-Object -Property bytes -Sum).Sum
        $errors = @($results | Where-Object { $_.error } | ForEach-Object { $_.error })
        $foregroundAfter = Get-ForegroundWindow
        $summaryPath = Join-Path $EvidenceDirectory 'real-soak.json'
        [ordered]@{
            target = $RealTarget
            players = $RealPlayers
            seconds = $RealSeconds
            total_bytes = $totalBytes
            player_results = $results
            player_errors = $errors
            samples = $samples
            foreground_before = $foregroundBefore.ToString()
            foreground_after = $foregroundAfter.ToString()
        } | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $summaryPath -Encoding UTF8
        Add-Step 'real-soak' ($(if ($totalBytes -gt 0) { 'ok' } else { 'no-bytes' })) @{ seconds = $RealSeconds; players = $RealPlayers; bytes = $totalBytes; errors = $errors }
        if ($totalBytes -le 0) { throw 'real relay soak received no media bytes; treat the live-platform link as unverified.' }
    } finally {
        if (-not $relay.HasExited) { $relay.Kill(); $relay.WaitForExit(10000) | Out-Null }
        Remove-Item -LiteralPath $temporaryRoot -Recurse -Force -ErrorAction SilentlyContinue
        $script:summary.foreground_before = $foregroundBefore.ToString()
        $script:summary.foreground_after = (Get-ForegroundWindow).ToString()
    }
}
try {
    if ($Mode -eq 'real-soak') { Invoke-RealSoak } else { Invoke-Synthetic }
    $summary.finished = (Get-Date).ToString('o')
    $summary.status = 'completed'
} catch {
    Add-Step 'failure' 'error' @{ message = $_.Exception.Message }
    $summary.finished = (Get-Date).ToString('o')
    $summary.status = 'failed'
    throw
} finally {
    $summary | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $EvidenceDirectory 'stress-report.json') -Encoding UTF8
}
