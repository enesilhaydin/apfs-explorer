# scripts/win-large.ps1
# Large sizes through apfs-mount.exe on a physical drive (a real USB drive,
# or a VHD for a dry run): time, throughput and the mount process's memory
# for each step, every result checked by hash. Prints one line per step,
# "PASS <step>: ..." or "FAIL <step>: ...".
#
#   L1  copy a 1 GiB file in and hash it back
#   L2  copy a 4 GiB file in and hash it back
#   L3  copy 10 000 small files into one folder, list it, hash a sample
#   L4  edit a 2 GiB file in place: append, write in the middle, 1000
#       scattered writes, cut it short; compared with the same edits made
#       to a local copy
#   L5  100 000 empty files in one folder: create, list, rename 1000,
#       delete 1000
#
# Usage (elevated):
#   powershell -ExecutionPolicy Bypass -File scripts\win-large.ps1 `
#       -Exe C:\path\apfs-mount.exe -Disk 3 -Letter U: [-Only L3] [-Small]
# -Small divides the sizes by 16 for a quick dry run.

param(
    [string]$Exe = "C:\apfs\audit\target\release\apfs-mount.exe",
    [int]$Disk = 3,
    [string]$Letter = "U:",
    [string]$Only = "",
    [switch]$Small
)
$ErrorActionPreference = "Stop"
$work = Join-Path $env:TEMP "apfs-large"
New-Item -ItemType Directory -Force $work | Out-Null
$scale = if ($Small) { 16 } else { 1 }

function Start-Mount {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = "--drive $Disk --mount $Letter --writable"
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    for ($i = 0; $i -lt 120; $i++) {
        if (Test-Path "$Letter\") { return $p }
        if ($p.HasExited) { throw "apfs-mount exited with $($p.ExitCode) before mounting" }
        Start-Sleep -Milliseconds 250
    }
    $p.Kill()
    throw "the drive letter did not appear within 30 s"
}

function Stop-Mount($p) {
    if ($p.HasExited) { return }
    $p.StandardInput.WriteLine("")
    if (-not $p.WaitForExit(300000)) { $p.Kill(); throw "apfs-mount did not stop within 5 min" }
}

# Peak private memory of the mount process while a step runs, sampled every
# half second by a job (the process's own peak counter covers working set
# only).
function Start-Sampler($p, [string]$out) {
    Start-Job -ArgumentList $p.Id, $out -ScriptBlock {
        param($id, $out)
        $max = 0
        while ($true) {
            $q = Get-Process -Id $id -ErrorAction SilentlyContinue
            if (-not $q) { break }
            if ($q.PrivateMemorySize64 -gt $max) { $max = $q.PrivateMemorySize64; Set-Content $out $max }
            Start-Sleep -Milliseconds 500
        }
    }
}

function Step($p, [string]$name, [scriptblock]$body) {
    $out = Join-Path $work "peak.txt"
    Set-Content $out 0
    $job = Start-Sampler $p $out
    try {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $detail = & $body
        $sw.Stop()
        Start-Sleep -Milliseconds 600
        $peak = [int64](Get-Content $out) / 1MB
        Write-Output ("PASS {0}: {1} | {2:N1} s | mount private peak {3:N0} MB" -f $name, $detail, $sw.Elapsed.TotalSeconds, $peak)
    } catch {
        Write-Output ("FAIL {0}: {1}" -f $name, $_.Exception.Message)
    } finally {
        Stop-Job $job -ErrorAction SilentlyContinue
        Remove-Job $job -Force -ErrorAction SilentlyContinue
    }
}

function Make-File([string]$path, [int64]$bytes) {
    if ((Test-Path $path) -and (Get-Item $path).Length -eq $bytes) { return }
    $f = [System.IO.File]::Create($path)
    $buf = New-Object byte[] (4MB)
    $rng = New-Object System.Random 7
    $rng.NextBytes($buf)
    for ([int64]$done = 0; $done -lt $bytes; $done += $buf.Length) {
        $buf[0] = [byte](($done / $buf.Length) % 256)
        $f.Write($buf, 0, [int][Math]::Min([int64]$buf.Length, [int64]($bytes - $done)))
    }
    $f.Close()
}

function Hash([string]$path) { (Get-FileHash -Algorithm SHA256 $path).Hash }

function Copy-Check($p, [string]$name, [int64]$bytes) {
    $srcFile = Join-Path $work "$name.bin"
    Make-File $srcFile $bytes
    $want = Hash $srcFile
    Step $p $name {
        $t = Measure-Command { Copy-Item $srcFile "$Letter\$name.bin" }
        $got = Hash "$Letter\$name.bin"
        if ($got -ne $want) { throw "hash differs after the copy" }
        "{0:N0} MiB, copy {1:N0} MB/s" -f ($bytes / 1MB), ($bytes / 1MB / $t.TotalSeconds)
    }
}

function Test-L3($p) {
    $n = [int](10000 / $scale)
    $src = Join-Path $work "small$n"
    # Made again when any file is missing: a run stopped while making them
    # left a part, and the copy then had only that part to copy.
    if (@(Get-ChildItem $src -File -ErrorAction SilentlyContinue).Count -ne $n) {
        New-Item -ItemType Directory -Force $src | Out-Null
        $rng = New-Object System.Random 3
        for ($i = 0; $i -lt $n; $i++) {
            $b = New-Object byte[] ($rng.Next(4KB, 64KB))
            $rng.NextBytes($b)
            [System.IO.File]::WriteAllBytes("$src\s$i.bin", $b)
        }
    }
    Step $p "L3 $n small files" {
        # A run before this one left its copy: start from an empty folder.
        if (Test-Path "$Letter\small") { Remove-Item "$Letter\small" -Recurse -Force }
        New-Item -ItemType Directory "$Letter\small" | Out-Null
        $t = Measure-Command { Copy-Item "$src\*" "$Letter\small\" }
        $tl = Measure-Command { $count = @(Get-ChildItem "$Letter\small").Count }
        if ($count -ne $n) { throw "listed $count of $n" }
        for ($i = 0; $i -lt $n; $i += [int]($n / 200)) {
            if ((Hash "$src\s$i.bin") -ne (Hash "$Letter\small\s$i.bin")) { throw "s$i.bin differs" }
        }
        "copy {0:N1} s ({1:N0} files/s), list {2:N0} ms" -f $t.TotalSeconds, ($n / $t.TotalSeconds), $tl.TotalMilliseconds
    }
}

function Test-L4($p) {
    [int64]$bytes = 2GB / $scale
    $local = Join-Path $work "edit.bin"
    Make-File $local $bytes
    Step $p "L4 edits in a $([int]($bytes / 1MB)) MiB file" {
        Copy-Item $local "$Letter\edit.bin"
        $times = @()
        foreach ($target in "$Letter\edit.bin", $local) {
            $rng = New-Object System.Random 11
            $t = Measure-Command {
                $fs = [System.IO.File]::Open($target, 'Open', 'ReadWrite')
                $b = New-Object byte[] (1MB); $rng.NextBytes($b)
                $fs.Seek(0, 'End') | Out-Null; $fs.Write($b, 0, $b.Length)
                $b = New-Object byte[] 4096; $rng.NextBytes($b)
                $fs.Seek($bytes / 2, 'Begin') | Out-Null; $fs.Write($b, 0, $b.Length)
                for ($k = 0; $k -lt 1000; $k++) {
                    $fs.Seek([int64]($rng.NextDouble() * ($bytes - 4096)), 'Begin') | Out-Null
                    $fs.Write($b, 0, 4096)
                }
                $fs.SetLength([int64]($bytes * 3 / 4) + 12345)
                $fs.Close()
            }
            $times += $t.TotalMilliseconds
        }
        if ((Hash "$Letter\edit.bin") -ne (Hash $local)) { throw "the edited file differs from the local copy" }
        "edits {0:N0} ms (local NTFS {1:N0} ms)" -f $times[0], $times[1]
    }
    Remove-Item $local
}

function Test-L5($p) {
    $n = [int](100000 / $scale)
    Step $p "L5 $n entries in one folder" {
        $d = "$Letter\wide"
        New-Item -ItemType Directory $d | Out-Null
        $tc = Measure-Command { for ($i = 0; $i -lt $n; $i++) { [System.IO.File]::WriteAllBytes("$d\e$i", [byte[]]@()) } }
        $tl = Measure-Command { $count = [System.IO.Directory]::GetFiles($d).Count }
        if ($count -ne $n) { throw "listed $count of $n" }
        $tr = Measure-Command { for ($i = 0; $i -lt 1000; $i++) { [System.IO.File]::Move("$d\e$i", "$d\r$i") } }
        $td = Measure-Command { for ($i = 1000; $i -lt 2000; $i++) { [System.IO.File]::Delete("$d\e$i") } }
        $count = [System.IO.Directory]::GetFiles($d).Count
        if ($count -ne $n - 1000) { throw "listed $count of $($n - 1000) after the deletes" }
        "create {0:N1} s, list {1:N0} ms, rename 1000 {2:N0} ms, delete 1000 {3:N0} ms" -f $tc.TotalSeconds, $tl.TotalMilliseconds, $tr.TotalMilliseconds, $td.TotalMilliseconds
    }
}

Write-Output "apfs-mount: $Exe  disk: $Disk  letter: $Letter  scale: 1/$scale"
$p = Start-Mount
try {
    if (-not $Only -or $Only -eq "L1") { Copy-Check $p "L1" ([int64](1GB / $scale)) }
    if (-not $Only -or $Only -eq "L2") { Copy-Check $p "L2" ([int64](4GB / $scale)) }
    if (-not $Only -or $Only -eq "L3") { Test-L3 $p }
    if (-not $Only -or $Only -eq "L4") { Test-L4 $p }
    if (-not $Only -or $Only -eq "L5") { Test-L5 $p }
} finally {
    Stop-Mount $p
}
# Everything again from a fresh mount: what was written is on the disk.
$p = Start-Mount
try {
    $files = @(Get-ChildItem "$Letter\" -Recurse -File).Count
    Write-Output "remounted: $files files"
} finally {
    Stop-Mount $p
}
