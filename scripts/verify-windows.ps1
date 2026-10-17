param(
    [string] $SourceManifest = '',
    [string] $BuildOutputDir = ''
)

# Build reviewed sibling sources without replacing or launching a running server.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$serverRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$sourceParent = Split-Path -Parent $serverRoot
if ($SourceManifest -eq '') {
    $SourceManifest = Join-Path $sourceParent 'SOURCE-MANIFEST.json'
}
if ($BuildOutputDir -eq '') {
    $BuildOutputDir = Join-Path $serverRoot 'target\capture-0.1.3'
}
if (-not (Test-Path -LiteralPath $SourceManifest -PathType Leaf)) {
    throw 'SOURCE-MANIFEST.json is required; use the complete reviewed source bundle.'
}
$manifestFile = (Resolve-Path -LiteralPath $SourceManifest).Path
$manifestRoot = Split-Path -Parent $manifestFile
$reviewedServer = (Resolve-Path -LiteralPath (Join-Path $manifestRoot 'qemu-screen-grab-server')).Path
if ($reviewedServer -ne $serverRoot) {
    throw 'The manifest describes a different server checkout.'
}
$manifest = Get-Content -LiteralPath $manifestFile -Raw | ConvertFrom-Json
if ($manifest.format -ne 1) { throw 'Unsupported source manifest format.' }
$expectedFiles = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($entry in $manifest.files) {
    $relativeName = ([string] $entry.path).Replace('\', '/')
    if ([IO.Path]::IsPathRooted($relativeName) -or $relativeName -match '(^|/)\.\.(/|$)' -or
        $relativeName -notmatch '^(xsar|qemu-screen-grab-server)/') {
        throw 'Unsafe source manifest path.'
    }
    if (-not $expectedFiles.Add($relativeName)) { throw 'Duplicate source manifest path.' }
}

function Test-ServerRuntimeLogName {
    param([string] $Name)
    $logName = [regex]::Match($Name,
        '\Aqemu-screen-grab-server-(?<stamp>[0-9]{8}-[0-9]{6}-[0-9]{3})-pid(?<process>[1-9][0-9]{0,9})(?:-0(?:0[1-9]|[1-9][0-9]))?\.log\z',
        [Text.RegularExpressions.RegexOptions]::CultureInvariant)
    if (-not $logName.Success) { return $false }
    $localStamp = [DateTime]::MinValue
    if (-not [DateTime]::TryParseExact($logName.Groups['stamp'].Value,
        'yyyyMMdd-HHmmss-fff', [Globalization.CultureInfo]::InvariantCulture,
        [Globalization.DateTimeStyles]::None, [ref] $localStamp)) { return $false }
    $loggedProcessId = [uint32] 0
    return [uint32]::TryParse($logName.Groups['process'].Value, [ref] $loggedProcessId)
}

function Get-ReviewedSourceFiles {
    param([string] $Directory, [string] $Prefix)
    foreach ($sourceEntry in Get-ChildItem -LiteralPath $Directory -Force) {
        if (($Prefix -eq 'xsar' -or $Prefix -eq 'qemu-screen-grab-server') -and
            ($sourceEntry.Name -eq '.git' -or $sourceEntry.Name -eq 'target')) { continue }
        if (($sourceEntry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Source junction/symlink is not permitted: $($sourceEntry.FullName)"
        }
        # Only generated regular runtime logs in this exact root are non-source.
        if ($Prefix -ceq 'qemu-screen-grab-server' -and -not $sourceEntry.PSIsContainer -and
            (Test-ServerRuntimeLogName $sourceEntry.Name)) { continue }
        $relativeName = $Prefix + '/' + $sourceEntry.Name
        if ($sourceEntry.PSIsContainer) {
            Get-ReviewedSourceFiles $sourceEntry.FullName $relativeName
        }
        else { $relativeName }
    }
}

function Assert-ReviewedSource {
    foreach ($sourceRepo in @('xsar', 'qemu-screen-grab-server')) {
        $repoDirectory = Join-Path $manifestRoot $sourceRepo
        foreach ($relativeName in @(Get-ReviewedSourceFiles $repoDirectory $sourceRepo)) {
            if (-not $expectedFiles.Contains($relativeName)) {
                throw "Unlisted source file: $relativeName; use a fresh isolated extraction."
            }
        }
    }
    foreach ($entry in $manifest.files) {
        $sourceFile = Join-Path $manifestRoot ([string] $entry.path)
        if (-not (Test-Path -LiteralPath $sourceFile -PathType Leaf)) {
            throw "Missing reviewed source: $($entry.path)"
        }
        $actualHash = (Get-FileHash -LiteralPath $sourceFile -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualHash -ne $entry.sha256) { throw "Source hash mismatch: $($entry.path)" }
    }
}

Assert-ReviewedSource
New-Item -ItemType Directory -Path $BuildOutputDir -Force | Out-Null
$BuildOutputDir = (Resolve-Path -LiteralPath $BuildOutputDir).Path
$logDirectory = Join-Path $BuildOutputDir ('verification-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $logDirectory -ErrorAction Stop | Out-Null
$previousTarget = [Environment]::GetEnvironmentVariable('CARGO_TARGET_DIR', 'Process')
$previousDocs = [Environment]::GetEnvironmentVariable('RUSTDOCFLAGS', 'Process')
$previousDirectory = Get-Location
$checks = New-Object System.Collections.Generic.List[object]

function Invoke-ReviewedCheck {
    param([string] $Name, [string[]] $RustupArguments)
    $logFile = Join-Path $logDirectory ($Name + '.log')
    $stdoutFile = Join-Path $logDirectory ($Name + '.stdout.log')
    $stderrFile = Join-Path $logDirectory ($Name + '.stderr.log')
    Write-Host "Running $Name; log: $logFile"
    # These fixed arguments contain no spaces or command-line quoting characters.
    foreach ($argument in $RustupArguments) {
        if ($argument -notmatch '^[A-Za-z0-9_.=:+-]+$') {
            throw 'Unexpected argument in the reviewed verification command.'
        }
    }
    $processInfo = New-Object Diagnostics.ProcessStartInfo
    $processInfo.FileName = (Get-Command rustup -CommandType Application -ErrorAction Stop).Source
    $processInfo.Arguments = $RustupArguments -join ' '
    $processInfo.WorkingDirectory = $serverRoot
    $processInfo.UseShellExecute = $false
    $processInfo.RedirectStandardOutput = $true
    $processInfo.RedirectStandardError = $true
    $processInfo.StandardOutputEncoding = [Text.Encoding]::UTF8
    $processInfo.StandardErrorEncoding = [Text.Encoding]::UTF8
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $processInfo
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        if (-not $process.Start()) { throw "Unable to start $Name" }
        # Drain both streams concurrently, without PowerShell 5.1 treating ordinary
        # Cargo stderr progress as a terminating NativeCommandError.
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $process.WaitForExit()
        $stdoutText = $stdoutTask.GetAwaiter().GetResult()
        $stderrText = $stderrTask.GetAwaiter().GetResult()
        $commandExit = $process.ExitCode
        [IO.File]::WriteAllText($stdoutFile, $stdoutText, [Text.Encoding]::UTF8)
        [IO.File]::WriteAllText($stderrFile, $stderrText, [Text.Encoding]::UTF8)
        [IO.File]::WriteAllText($logFile, $stdoutText + $stderrText, [Text.Encoding]::UTF8)
        Write-Host $stdoutText -NoNewline
        Write-Host $stderrText -NoNewline
    }
    finally {
        $watch.Stop()
        $process.Dispose()
    }
    $checks.Add([pscustomobject]@{
        name = $Name; argv = $RustupArguments; exit_code = $commandExit
        elapsed_seconds = $watch.Elapsed.TotalSeconds; log = $logFile
        stdout_log = $stdoutFile; stderr_log = $stderrFile
    })
    if ($commandExit -ne 0) { throw "$Name failed with exit $commandExit; retained $logFile" }
}

try {
    Set-Location -LiteralPath $serverRoot
    $env:CARGO_TARGET_DIR = $BuildOutputDir
    Invoke-ReviewedCheck '01-rustc' @('run', 'nightly', 'rustc', '--version', '--verbose')
    $hostMatches = @(Select-String -LiteralPath (Join-Path $logDirectory '01-rustc.stdout.log') -Pattern '^host: (\S+)$')
    if ($hostMatches.Count -ne 1) { throw 'Unable to identify the installed compiler host.' }
    $nativeHost = $hostMatches[0].Matches[0].Groups[1].Value
    if ($nativeHost -notmatch '-windows-') { throw 'This verifier requires a native Windows host toolchain.' }
    Invoke-ReviewedCheck '02-cargo' @('run', 'nightly', 'cargo', '--version')
    Invoke-ReviewedCheck '03-check' @('run', 'nightly', 'cargo', 'check', '--locked', '--target', $nativeHost, '--all-targets')
    Invoke-ReviewedCheck '04-tests' @('run', 'nightly', 'cargo', 'test', '--locked', '--target', $nativeHost, '--', '--include-ignored', '--test-threads=2')
    $env:RUSTDOCFLAGS = '-D warnings'
    Invoke-ReviewedCheck '05-docs' @('run', 'nightly', 'cargo', 'doc', '--locked', '--target', $nativeHost, '--no-deps', '--document-private-items')
    [Environment]::SetEnvironmentVariable('RUSTDOCFLAGS', $previousDocs, 'Process')
    Invoke-ReviewedCheck '06-clippy' @('run', 'nightly', 'cargo', 'clippy', '--locked', '--target', $nativeHost, '--all-targets', '--', '-D', 'warnings')
    Invoke-ReviewedCheck '07-release' @('run', 'nightly', 'cargo', 'build', '--locked', '--target', $nativeHost, '--release', '--message-format=json-render-diagnostics')
    Assert-ReviewedSource
    $artifacts = @()
    foreach ($recordLine in Get-Content -LiteralPath (Join-Path $logDirectory '07-release.stdout.log')) {
        if (-not $recordLine.StartsWith('{')) { continue }
        try { $record = $recordLine | ConvertFrom-Json } catch { continue }
        if ($record.reason -eq 'compiler-artifact' -and $record.target.name -eq 'qemu_screen_grab_server' -and
            $record.target.kind -contains 'bin' -and $record.executable) {
            $artifacts += [string] $record.executable
        }
    }
    $artifacts = @($artifacts | Select-Object -Unique)
    if ($artifacts.Count -ne 1) { throw 'Cargo did not identify exactly one server executable.' }
    $result = [pscustomobject]@{
        complete = $true; server_version = '0.1.3'; xsar_version = '0.4.2-candidate.2'
        host = $nativeHost
        source_manifest = $manifestFile; checks = $checks
        executable = $artifacts[0]
        sha256 = (Get-FileHash -LiteralPath $artifacts[0] -Algorithm SHA256).Hash.ToLowerInvariant()
        native_live_capture = 'NOT RUN'; deployment = 'NOT PERFORMED'
    }
    $result | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $logDirectory 'result.json') -Encoding UTF8
    Write-Host "Build complete. Executable: $($result.executable)"
    Write-Host "SHA-256: $($result.sha256)"
    Write-Host 'No server was started or installed; live GDI checks remain separate.'
}
catch {
    [pscustomobject]@{ complete = $false; error = $_.Exception.Message; checks = $checks } |
        ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $logDirectory 'result.json') -Encoding UTF8
    throw
}
finally {
    [Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', $previousTarget, 'Process')
    [Environment]::SetEnvironmentVariable('RUSTDOCFLAGS', $previousDocs, 'Process')
    Set-Location -LiteralPath $previousDirectory
}
