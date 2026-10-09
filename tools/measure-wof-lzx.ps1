# SPDX-License-Identifier: AGPL-3.0-or-later
param(
    [Parameter(Mandatory = $true)][string]$Source,
    [Parameter(Mandatory = $true)][string]$Scratch,
    [switch]$KeepCopy
)

$ErrorActionPreference = 'Stop'

Add-Type @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class FlummoxAllocation {
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern uint GetCompressedFileSizeW(string name, out uint high);

    // The extended-length form lifts the MAX_PATH limit, so deep game trees are measured.
    private static string Long(string path) {
        if (path.StartsWith(@"\\?\")) return path;
        if (path.StartsWith(@"\\")) return @"\\?\UNC\" + path.Substring(2);
        return @"\\?\" + path;
    }

    public static ulong Bytes(string path) {
        uint high;
        uint low = GetCompressedFileSizeW(Long(path), out high);
        if (low == 0xffffffff && Marshal.GetLastWin32Error() != 0) {
            throw new Win32Exception(Marshal.GetLastWin32Error(), path);
        }
        return ((ulong)high << 32) | low;
    }
}
'@

function Get-Corpus([string]$Root) {
    $rootPath = [IO.Path]::GetFullPath($Root).TrimEnd('\')
    $files = @{}
    Get-ChildItem -LiteralPath $rootPath -File -Recurse -Force |
        Where-Object { $null -eq $_.LinkType } | ForEach-Object {
        $relative = $_.FullName.Substring($rootPath.Length + 1).Replace('\', '/')
        $files.Add($relative, $_)
    }
    $relativePaths = [string[]]$files.Keys
    [Array]::Sort($relativePaths, [StringComparer]::Ordinal)
    $hash = [Security.Cryptography.IncrementalHash]::CreateHash(
        [Security.Cryptography.HashAlgorithmName]::SHA256)
    $utf8 = [Text.UTF8Encoding]::new($false)
    $buffer = [byte[]]::new(1024 * 1024)
    [UInt64]$logical = 0
    [UInt64]$allocated = 0
    foreach ($relative in $relativePaths) {
        $file = $files[$relative]
        $pathBytes = $utf8.GetBytes($relative)
        $hash.AppendData([BitConverter]::GetBytes([UInt64]$pathBytes.Length))
        $hash.AppendData($pathBytes)
        $hash.AppendData([BitConverter]::GetBytes([UInt64]$file.Length))
        $stream = [IO.File]::OpenRead($file.FullName)
        try {
            while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                $hash.AppendData($buffer, 0, $count)
            }
        } finally {
            $stream.Dispose()
        }
        $logical += [UInt64]$file.Length
        $allocated += [FlummoxAllocation]::Bytes($file.FullName)
    }
    [PSCustomObject]@{
        Files = [UInt64]$relativePaths.Count
        LogicalBytes = $logical
        AllocatedBytes = $allocated
        CorpusSha256 = ([BitConverter]::ToString($hash.GetHashAndReset())).Replace('-', '').ToLowerInvariant()
    }
}

$sourcePath = (Resolve-Path -LiteralPath $Source).Path
$scratchPath = (Resolve-Path -LiteralPath $Scratch).Path
$sourcePrefix = $sourcePath.TrimEnd('\') + '\'
$scratchPrefix = $scratchPath.TrimEnd('\') + '\'
if ($scratchPrefix.StartsWith($sourcePrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Scratch must be outside the source tree'
}
$drive = [IO.DriveInfo]::new([IO.Path]::GetPathRoot($scratchPath))
if ($drive.DriveFormat -ne 'NTFS') {
    throw 'The Windows WOF scratch directory must be on NTFS'
}
# Controls: the harness must be able to fail. Zeros must shrink under compact.exe and
# random bytes must not. If either is wrong the numbers below mean nothing.
$controlPath = Join-Path $scratchPath ("flummox-wof-control-" + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($controlPath) | Out-Null
try {
    [IO.File]::WriteAllBytes((Join-Path $controlPath 'zeros.bin'), [byte[]]::new(4MB))
    $noise = [byte[]]::new(4MB)
    [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($noise)
    [IO.File]::WriteAllBytes((Join-Path $controlPath 'random.bin'), $noise)
    $zerosBefore = [FlummoxAllocation]::Bytes((Join-Path $controlPath 'zeros.bin'))
    $noiseBefore = [FlummoxAllocation]::Bytes((Join-Path $controlPath 'random.bin'))
    & compact.exe /c "/s:$controlPath" /a /f /exe:lzx |
        ForEach-Object { [Console]::Error.WriteLine($_) }
    if ($LASTEXITCODE -ne 0) {
        throw "compact.exe failed on the control files with exit code $LASTEXITCODE"
    }
    $zerosAfter = [FlummoxAllocation]::Bytes((Join-Path $controlPath 'zeros.bin'))
    $noiseAfter = [FlummoxAllocation]::Bytes((Join-Path $controlPath 'random.bin'))
    if ($zerosAfter -ge $zerosBefore) {
        throw "Control failed: a file of zeros did not shrink ($zerosBefore -> $zerosAfter)"
    }
    if ($noiseAfter -lt $noiseBefore) {
        throw "Control failed: random data shrank ($noiseBefore -> $noiseAfter)"
    }
} finally {
    Remove-Item -LiteralPath $controlPath -Recurse -Force
}

$copyPath = Join-Path $scratchPath ("flummox-wof-" + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($copyPath) | Out-Null

try {
    & robocopy.exe $sourcePath $copyPath /E /COPY:DAT /DCOPY:DAT /SL /R:0 /W:0 |
        ForEach-Object { [Console]::Error.WriteLine($_) }
    if ($LASTEXITCODE -gt 7) {
        throw "robocopy failed with exit code $LASTEXITCODE"
    }
    $before = Get-Corpus $copyPath
    $started = [Diagnostics.Stopwatch]::StartNew()
    & compact.exe /c "/s:$copyPath" /a /f /exe:lzx |
        ForEach-Object { [Console]::Error.WriteLine($_) }
    if ($LASTEXITCODE -ne 0) {
        throw "compact.exe failed with exit code $LASTEXITCODE"
    }
    $started.Stop()
    $after = Get-Corpus $copyPath
    if ($before.Files -ne $after.Files -or
        $before.LogicalBytes -ne $after.LogicalBytes -or
        $before.CorpusSha256 -ne $after.CorpusSha256) {
        throw 'The copied corpus changed during WOF compression'
    }
    [PSCustomObject]@{
        method = 'Windows compact.exe WOF/LZX'
        corpus_sha256 = $after.CorpusSha256
        files = $after.Files
        logical_bytes = $after.LogicalBytes
        allocated_bytes_before = $before.AllocatedBytes
        allocated_bytes_after = $after.AllocatedBytes
        compression_ns = [UInt64]($started.Elapsed.TotalMilliseconds * 1000000)
    } | ConvertTo-Json
} finally {
    if (-not $KeepCopy -and (Test-Path -LiteralPath $copyPath)) {
        Remove-Item -LiteralPath $copyPath -Recurse -Force
    }
}
