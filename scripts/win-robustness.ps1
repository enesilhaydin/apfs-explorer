# scripts/win-robustness.ps1
# Fault and endurance scenarios for apfs-mount.exe on a physical drive (a
# real USB drive, or a VHD holding an APFS image for a dry run). Each
# scenario prints "PASS <name>" or "FAIL <name>: <why>".
#
#   S1  the disk goes offline in the middle of a copy
#   S2  the mount process is killed in the middle of a copy
#   S3  ten mount / list / unmount cycles in a row
#   S4  a device that is not APFS, and a truncated image: a clean error
#   S5  soak: a mixed workload for -SoakMinutes, every file read back and
#       checked, memory sampled; then a remount and a second check
#   S6  a read-only mount refuses writes
#
# After every scenario "<S>-leak" checks that it left no process stuck in
# the kernel and no file pages that cannot be written.
#
# After a run, take the drive to a Mac and run `fsck_apfs -n` on it.
#
# Usage (elevated):
#   powershell -ExecutionPolicy Bypass -File scripts\win-robustness.ps1 `
#       -Exe C:\path\apfs-mount.exe -Disk 3 -Letter U: -SoakMinutes 60

param(
    [string]$Exe = "C:\apfs\audit\target\release\apfs-mount.exe",
    [int]$Disk = 3,
    [string]$Letter = "U:",
    [int]$SoakMinutes = 5,
    # The soak keeps at most this much file data alive at once.
    [int]$SoakBudgetMiB = 256,
    # The soak fails when the mount's private memory passes this.
    [int]$MemoryLimitMiB = 96,
    # A scenario fails when the system's modified (unwritten) page list is
    # still this much larger than before it, once it has had time to drain.
    [int]$LeakLimitMiB = 256,
    [string]$Only = ""
)
$ErrorActionPreference = "Stop"
$work = Join-Path $env:TEMP "apfs-robustness"
New-Item -ItemType Directory -Force $work | Out-Null

function Result([string]$tag, [string]$name, [string]$detail) {
    Write-Output ("{0} {1}: {2}" -f $tag, $name, $detail)
}

function Start-Mount([switch]$ReadOnly) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = "--drive $Disk --mount $Letter" + $(if ($ReadOnly) { "" } else { " --writable" })
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

# The graceful stop: apfs-mount commits the open write group and unmounts
# when its standard input gets a line.
function Stop-Mount($p, [int]$TimeoutSec = 60) {
    if ($p.HasExited) { return }
    $p.StandardInput.WriteLine("")
    if (-not $p.WaitForExit($TimeoutSec * 1000)) {
        $p.Kill()
        throw "apfs-mount did not stop within $TimeoutSec s"
    }
}

function Wait-LetterGone([int]$TimeoutSec = 30) {
    for ($i = 0; $i -lt $TimeoutSec * 4; $i++) {
        if (-not (Test-Path "$Letter\")) { return }
        Start-Sleep -Milliseconds 250
    }
    throw "$Letter still present $TimeoutSec s after the mount ended"
}

function Assert-NoOrphan {
    $left = @(Get-Process apfs-mount -ErrorAction SilentlyContinue)
    if ($left.Count -gt 0) {
        $left | Stop-Process -Force
        throw "$($left.Count) apfs-mount process(es) left behind"
    }
}

# A process that died of an exception, not of an orderly exit.
function Assert-NoCrash($p) {
    if ($p.HasExited -and ($p.ExitCode -band 0xC0000000) -eq 0xC0000000) {
        throw ("apfs-mount crashed: exit code 0x{0:X8}" -f $p.ExitCode)
    }
}

# What a request stuck in the kernel leaves behind: a process that has
# ended but cannot go away (no handles left, a thread still waiting), and
# file pages that can never be written. With WinFsp data caching on, copies
# of 4 GiB once left five such processes holding 20 GB of memory until
# Windows restarted.
$pseudo = "Idle", "System", "Secure System", "Registry", "Memory Compression"
function Get-Stuck {
    @(Get-Process | Where-Object { $_.HandleCount -eq 0 -and $pseudo -notcontains $_.Name } |
        ForEach-Object { $_.Id })
}
function Get-ModifiedMiB {
    [int]((Get-CimInstance Win32_PerfFormattedData_PerfOS_Memory).ModifiedPageListBytes / 1MB)
}
function Get-LeakBaseline { @{ Stuck = Get-Stuck; Modified = Get-ModifiedMiB } }
function Test-Leak([string]$name, $base) {
    $stuck = @(); $grown = 0
    # The modified page writer and process teardown take a few seconds.
    for ($i = 0; $i -lt 30; $i++) {
        $stuck = @(Get-Stuck | Where-Object { $base.Stuck -notcontains $_ })
        $grown = (Get-ModifiedMiB) - $base.Modified
        if ($stuck.Count -eq 0 -and $grown -le $LeakLimitMiB) { break }
        Start-Sleep -Seconds 1
    }
    if ($stuck.Count -gt 0) {
        Result "FAIL" "$name-leak" "$($stuck.Count) process(es) stuck in the kernel: $($stuck -join ' ')"
    } elseif ($grown -gt $LeakLimitMiB) {
        Result "FAIL" "$name-leak" "unwritten (modified) pages grew by $grown MiB"
    } else {
        Result "PASS" "$name-leak" "no stuck process; modified pages $grown MiB"
    }
}

function Set-Online([bool]$online) {
    Set-Disk -Number $Disk -IsOffline (-not $online)
    Start-Sleep -Seconds 2
}

# Source files the copies use: 200 small files and one of 128 MiB. Made
# again when any is missing: a run stopped while making them left the
# folder empty, and every copy after it copied nothing.
$src = Join-Path $work "src"
if (@(Get-ChildItem $src -File -ErrorAction SilentlyContinue).Count -ne 201) {
    New-Item -ItemType Directory -Force $src | Out-Null
    for ($i = 0; $i -lt 200; $i++) {
        $b = New-Object byte[] (4096 + $i * 37)
        for ($j = 0; $j -lt $b.Length; $j += 61) { $b[$j] = [byte](($i + $j) % 251) }
        [System.IO.File]::WriteAllBytes("$src\f$i.bin", $b)
    }
    $f = [System.IO.File]::Create("$src\big.bin")
    $buf = New-Object byte[] (1MB)
    for ($m = 0; $m -lt 128; $m++) { $buf[0] = [byte]$m; $f.Write($buf, 0, $buf.Length) }
    $f.Close()
}

# A copy into the mount, run as a job so the caller can fault the mount
# while it is going on.
function Start-Copy([string]$dir) {
    Start-Job -ArgumentList $src, "$Letter\$dir" -ScriptBlock {
        param($from, $to)
        try {
            New-Item -ItemType Directory -Force $to | Out-Null
            Copy-Item "$from\*" $to -ErrorAction Stop
            "copied"
        } catch { "failed: " + $_.Exception.Message }
    }
}

# Return once the copy into `dir` has put some files there but not all, so
# the fault lands in the middle of it.
function Wait-CopyUnderway([string]$dir) {
    for ($i = 0; $i -lt 400; $i++) {
        $n = @(Get-ChildItem "$Letter\$dir" -ErrorAction SilentlyContinue).Count
        if ($n -ge 20) { return }
        Start-Sleep -Milliseconds 25
    }
    throw "the copy into $dir did not get going"
}

function Test-S1 {
    $name = "S1 disk offline during a copy"
    # Windows refuses to take removable media offline; on a USB stick this
    # scenario is a pull of the drive by hand instead.
    if ((Get-Disk -Number $Disk).BusType -eq "USB") {
        Result "SKIP" $name "removable drive: pull it out during a copy by hand instead"
        return
    }
    $p = $null
    try {
        $p = Start-Mount
        $job = Start-Copy "s1"
        Wait-CopyUnderway "s1"
        Set-Online $false
        if (-not (Wait-Job $job -Timeout 120)) { throw "the copy hung after the disk went offline" }
        $copy = Receive-Job $job
        Assert-NoCrash $p
        try { Stop-Mount $p 30 } catch { }
        if (-not $p.HasExited) { $p.Kill() }
        Wait-LetterGone
        Set-Online $true
        $p = Start-Mount
        $n = @(Get-ChildItem "$Letter\").Count
        Stop-Mount $p
        Wait-LetterGone
        Assert-NoOrphan
        Result "PASS" $name "copy: $copy; remounted, root lists $n entries"
    } catch {
        Result "FAIL" $name $_.Exception.Message
    } finally {
        Set-Disk -Number $Disk -IsOffline $false -ErrorAction SilentlyContinue
        if ($p -and -not $p.HasExited) { $p.Kill() }
    }
}

function Test-S2 {
    $name = "S2 mount killed during a copy"
    $p = $null
    try {
        $p = Start-Mount
        $job = Start-Copy "s2"
        Wait-CopyUnderway "s2"
        $p.Kill()
        if (-not (Wait-Job $job -Timeout 120)) { throw "the copy hung after the mount was killed" }
        $copy = Receive-Job $job
        Wait-LetterGone
        Assert-NoOrphan
        $p = Start-Mount
        $n = @(Get-ChildItem "$Letter\").Count
        Stop-Mount $p
        Wait-LetterGone
        Result "PASS" $name "copy: $copy; remounted, root lists $n entries"
    } catch {
        Result "FAIL" $name $_.Exception.Message
    } finally {
        if ($p -and -not $p.HasExited) { $p.Kill() }
    }
}

function Test-S3 {
    $name = "S3 ten mount cycles"
    try {
        $times = @()
        for ($k = 1; $k -le 10; $k++) {
            $t = Measure-Command {
                $p = Start-Mount -ReadOnly:($k % 2 -eq 0)
                Get-ChildItem "$Letter\" | Out-Null
                Stop-Mount $p
                Wait-LetterGone
            }
            $times += [int]$t.TotalMilliseconds
        }
        Assert-NoOrphan
        Result "PASS" $name ("ms per cycle: " + ($times -join " "))
    } catch {
        Result "FAIL" $name $_.Exception.Message
    }
}

function Test-S4 {
    $name = "S4 not APFS / truncated"
    try {
        $zero = Join-Path $work "zero.img"
        $cut = Join-Path $work "cut.img"
        [System.IO.File]::WriteAllBytes($zero, (New-Object byte[] (16MB)))
        $f = [System.IO.File]::Create($cut)
        $d = New-Object System.IO.FileStream("\\.\PhysicalDrive$Disk", [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
        $buf = New-Object byte[] (1MB)
        $d.Read($buf, 0, $buf.Length) | Out-Null
        $d.Close()
        $f.Write($buf, 0, $buf.Length)
        $f.Close()
        $out = @()
        foreach ($img in $zero, $cut) {
            $p = Start-Process -FilePath $Exe -ArgumentList "--image", $img, "--mount", $Letter -PassThru -WindowStyle Hidden
            if (-not $p.WaitForExit(20000)) { $p.Kill(); throw "apfs-mount kept running on $img" }
            if ($p.ExitCode -eq 0) { throw "apfs-mount reported success on $img" }
            if (($p.ExitCode -band 0xC0000000) -eq 0xC0000000) { throw ("crash 0x{0:X8} on {1}" -f $p.ExitCode, $img) }
            $out += "exit $($p.ExitCode)"
        }
        Assert-NoOrphan
        if (Test-Path "$Letter\") { throw "$Letter is mounted after a refused device" }
        Result "PASS" $name ($out -join ", ")
    } catch {
        Result "FAIL" $name $_.Exception.Message
    }
}

function Test-S5 {
    $name = "S5 soak $SoakMinutes min"
    $p = $null
    try {
        $p = Start-Mount
        $root = "$Letter\soak"
        New-Item -ItemType Directory -Force $root | Out-Null
        $rng = New-Object System.Random 1234
        $sha = [System.Security.Cryptography.SHA256]::Create()
        $live = @{}
        $ops = 0; $bad = 0; $samples = @()
        $budget = [int64]$SoakBudgetMiB * 1MB
        $liveBytes = { ($live.Keys | ForEach-Object { (Get-Item $_).Length } | Measure-Object -Sum).Sum }
        $held = [int64]0
        $end = (Get-Date).AddMinutes($SoakMinutes)
        $nextSample = Get-Date
        while ((Get-Date) -lt $end) {
            $roll = $rng.Next(100)
            $keys = @($live.Keys)
            if ($ops % 50 -eq 0) { $held = [int64](& $liveBytes) }
            if ($held -gt $budget -and $keys.Count -gt 0) { $roll = 85 }
            if ($roll -lt 45 -or $keys.Count -lt 20) {
                $size = if ($rng.Next(10) -eq 0) { $rng.Next(1MB, 8MB) } else { $rng.Next(1, 64KB) }
                $b = New-Object byte[] $size
                $rng.NextBytes($b)
                $fn = "$root\f$ops.bin"
                [System.IO.File]::WriteAllBytes($fn, $b)
                $live[$fn] = [Convert]::ToBase64String($sha.ComputeHash($b))
            } elseif ($roll -lt 70) {
                $fn = $keys[$rng.Next($keys.Count)]
                $b = [System.IO.File]::ReadAllBytes($fn)
                if ([Convert]::ToBase64String($sha.ComputeHash($b)) -ne $live[$fn]) { $bad++ }
            } elseif ($roll -lt 80) {
                $fn = $keys[$rng.Next($keys.Count)]
                $to = "$root\r$ops.bin"
                Move-Item $fn $to
                $live[$to] = $live[$fn]; $live.Remove($fn)
            } elseif ($roll -lt 90) {
                $fn = $keys[$rng.Next($keys.Count)]
                Remove-Item $fn
                $live.Remove($fn)
            } else {
                $fn = $keys[$rng.Next($keys.Count)]
                $extra = New-Object byte[] ($rng.Next(1, 20000))
                $rng.NextBytes($extra)
                $fs = [System.IO.File]::Open($fn, 'Open', 'ReadWrite')
                $fs.Seek($rng.Next([int][Math]::Max(1, $fs.Length)), 'Begin') | Out-Null
                $fs.Write($extra, 0, $extra.Length)
                $fs.Close()
                $live[$fn] = [Convert]::ToBase64String($sha.ComputeHash([System.IO.File]::ReadAllBytes($fn)))
            }
            $ops++
            if ((Get-Date) -ge $nextSample) {
                $p.Refresh()
                $samples += [int]($p.PrivateMemorySize64 / 1MB)
                $nextSample = (Get-Date).AddSeconds(30)
            }
        }
        Stop-Mount $p
        Wait-LetterGone
        $p = Start-Mount
        foreach ($fn in $live.Keys) {
            if (-not (Test-Path $fn)) { $bad++; continue }
            $b = [System.IO.File]::ReadAllBytes($fn)
            if ([Convert]::ToBase64String($sha.ComputeHash($b)) -ne $live[$fn]) { $bad++ }
        }
        Stop-Mount $p
        Wait-LetterGone
        Assert-NoOrphan
        $peak = ($samples | Measure-Object -Maximum).Maximum
        $detail = "ops $ops, files $($live.Count), mismatches $bad, private MB every 30 s: " +
            ($samples -join " ")
        if ($bad -gt 0) { throw $detail }
        # The block cache may hold up to 64 MiB of metadata by design; more
        # than that over the process's own few MB is growth with no bound.
        if ($peak -gt $MemoryLimitMiB) { throw "private memory reached $peak MB; $detail" }
        Result "PASS" $name $detail
    } catch {
        Result "FAIL" $name $_.Exception.Message
    } finally {
        if ($p -and -not $p.HasExited) { $p.Kill() }
    }
}

function Test-S6 {
    $name = "S6 a read-only mount refuses writes"
    $p = $null
    try {
        $p = Start-Mount -ReadOnly
        $probe = "$Letter\ro-probe-$PID.txt"
        $refused = $false
        try { [System.IO.File]::WriteAllText($probe, "x") } catch { $refused = $true }
        Stop-Mount $p
        Wait-LetterGone
        $p = Start-Mount -ReadOnly
        $left = Test-Path $probe
        Stop-Mount $p
        Wait-LetterGone
        if (-not $refused) { throw "a write on a read-only mount was accepted" }
        if ($left) { throw "a file written on a read-only mount is on the disk" }
        Result "PASS" $name "write refused, nothing on the disk"
    } catch {
        Result "FAIL" $name $_.Exception.Message
    } finally {
        if ($p -and -not $p.HasExited) { $p.Kill() }
    }
}

Write-Output "apfs-mount: $Exe  disk: $Disk  letter: $Letter"
Assert-NoOrphan
foreach ($s in "S1", "S2", "S3", "S4", "S6", "S5") {
    if ($Only -and $Only -ne $s) { continue }
    $base = Get-LeakBaseline
    & "Test-$s"
    Test-Leak $s $base
}
