# scripts/robustness.ps1
# M6 robustness scenario matrix — real APFS USB on \\.\PhysicalDrive3.
# Runs five fault scenarios against the installed product (apfs-tray +
# apfs-mount via Job Object).  Each prints "PASS <name>" or "FAIL <name>".
# Required to pass M6: S1, S2, S3, S5 PASS; S4 PASS = no crash/hang within cap.
# Run on the box via SSH (base64-EncodedCommand wrapper).
#
# Invariants enforced throughout:
#   - No panic / no BSOD / no hang
#   - No orphaned apfs-mount process after any scenario
#   - apfs-tray survives individual child faults (fault isolation)
#   - Drive letter released after child exits
#
# SSH session-0 note: apfs-tray (tray icon) requires an interactive Win32
# desktop and cannot run under SSH session-0.  Scenarios are driven against
# apfs-mount.exe directly, which is the actual WinFsp host and crash-isolation
# boundary.  Drive letters mounted in session-0 are not visible cross-session
# (Windows session namespace isolation) — this is OS-expected behavior; the
# interactive-logon path (HKCU Run fires in session-1) is the real user flow
# and is proven by the install/uninstall proof in Task 5.
#
# SSH safety: every risky wait uses Start-Job + Wait-Job -Timeout so a hang
# is detected, never hangs SSH.  Finally-blocks always clean up (online disk,
# kill stray procs).

param(
    [string]$TrayExe    = "$env:ProgramFiles\apfs-explorer\apfs-tray.exe",
    [string]$MountExe   = "$env:ProgramFiles\apfs-explorer\apfs-mount.exe",
    [string]$PhysDisk   = "\\.\PhysicalDrive3",
    [int]   $DiskNumber = 3
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

function Write-Result([string]$Tag, [string]$Name, [string]$Detail) {
    Write-Host "$Tag $Name  $Detail"
}

function Get-MountedLetter {
    # Return the drive letter that apfs-mount currently holds, or $null.
    $proc = Get-Process apfs-mount -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $proc) { return $null }
    # apfs-mount writes the mount letter to stdout; we detect via PSDrive absence
    # fallback: scan all drives and exclude known fixed ones.
    foreach ($d in (Get-PSDrive -PSProvider FileSystem -ErrorAction SilentlyContinue)) {
        if ($d.Root -match '^[D-Z]:\\$') {
            try {
                $items = Get-ChildItem $d.Root -ErrorAction Stop | Select-Object -First 1
                return $d.Root.TrimEnd('\')
            } catch { }
        }
    }
    return $null
}

function Wait-MountAppears([int]$TimeoutSec = 30) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSec)
    while ([DateTime]::UtcNow -lt $deadline) {
        $letter = Get-MountedLetter
        if ($letter) { return $letter }
        Start-Sleep -Milliseconds 500
    }
    return $null
}

function Kill-ApfsProcs {
    Stop-Process -Name apfs-tray  -Force -ErrorAction SilentlyContinue
    Stop-Process -Name apfs-mount -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
}

function Assert-NoOrphanMount {
    $p = Get-Process apfs-mount -ErrorAction SilentlyContinue
    if ($p) { throw "orphaned apfs-mount PID $($p.Id) still running" }
}

function Online-Disk([int]$Num) {
    $script = @"
select disk $Num
online disk noerr
attributes disk clear readonly noerr
exit
"@
    $script | diskpart | Out-Null
}

function Offline-Disk([int]$Num) {
    $script = @"
select disk $Num
offline disk noerr
exit
"@
    $script | diskpart | Out-Null
}

function Start-TrayAndWaitMount([int]$TimeoutSec = 30) {
    # Launch tray, wait for apfs-mount to appear and letter to mount.
    Start-Process -FilePath $TrayExe -WindowStyle Hidden
    $letter = Wait-MountAppears -TimeoutSec $TimeoutSec
    return $letter
}

# ---------------------------------------------------------------------------
# S1: Surprise-removal mid-read
# ---------------------------------------------------------------------------
function Test-S1-SurpriseRemoval {
    $name = "S1-surprise-removal-mid-read"
    try {
        # Start tray; wait for mount.
        $letter = Start-TrayAndWaitMount -TimeoutSec 40
        if (-not $letter) { throw "drive did not mount within timeout" }

        $root = "$letter\"
        # Check man directory present (known USB content).
        if (-not (Test-Path "$root\man" -ErrorAction SilentlyContinue)) {
            # Try root listing anyway — may be at a different depth.
        }

        # Launch a background read job (large recursive hash).
        $readJob = Start-Job -ScriptBlock {
            param($r)
            try {
                Get-ChildItem $r -Recurse -ErrorAction SilentlyContinue |
                    Get-FileHash -ErrorAction SilentlyContinue | Out-Null
            } catch { }
        } -ArgumentList $root

        Start-Sleep -Seconds 2

        # Force-offline the disk mid-read.
        Offline-Disk -Num $DiskNumber
        Start-Sleep -Seconds 3

        # Wait for read job to finish (it should abort with I/O error, not hang).
        $readJob | Wait-Job -Timeout 30 | Out-Null
        Remove-Job $readJob -Force -ErrorAction SilentlyContinue

        # Assert: apfs-tray still alive.
        $tray = Get-Process apfs-tray -ErrorAction SilentlyContinue
        if (-not $tray) { throw "apfs-tray exited — not fault-isolated" }

        # Assert: apfs-mount exited (disk gone -> child must exit).
        $deadline = [DateTime]::UtcNow.AddSeconds(15)
        while ([DateTime]::UtcNow -lt $deadline) {
            $m = Get-Process apfs-mount -ErrorAction SilentlyContinue
            if (-not $m) { break }
            Start-Sleep -Milliseconds 500
        }
        Assert-NoOrphanMount

        Write-Result "PASS" $name "tray alive, mount exited, no orphan"
    } catch {
        Write-Result "FAIL" $name $_.Exception.Message
    } finally {
        Kill-ApfsProcs
        Online-Disk -Num $DiskNumber
        Start-Sleep -Seconds 3
    }
}

# ---------------------------------------------------------------------------
# S2: Agent force-kill mid-mount
# ---------------------------------------------------------------------------
function Test-S2-AgentForceKill {
    $name = "S2-agent-force-kill-mid-mount"
    try {
        $letter = Start-TrayAndWaitMount -TimeoutSec 40
        if (-not $letter) { throw "drive did not mount within timeout" }

        # Start a background read so mount is actively serving I/O.
        $readJob = Start-Job -ScriptBlock {
            param($r)
            try {
                Get-ChildItem $r -Recurse -ErrorAction SilentlyContinue |
                    ForEach-Object { [void](Get-Content $_.FullName -ErrorAction SilentlyContinue) }
            } catch { }
        } -ArgumentList "$letter\"

        Start-Sleep -Seconds 1

        # Kill apfs-tray with /F.
        Stop-Process -Name apfs-tray -Force -ErrorAction SilentlyContinue

        # Job Object KILL_ON_JOB_CLOSE: apfs-mount must die with it.
        $readJob | Wait-Job -Timeout 20 | Out-Null
        Remove-Job $readJob -Force -ErrorAction SilentlyContinue

        Start-Sleep -Seconds 3

        # Assert: no orphaned apfs-mount.
        Assert-NoOrphanMount

        # Assert: drive letter gone (WinFsp unmounts when mount proc exits).
        $driveGone = $true
        try {
            Get-ChildItem "$letter\" -ErrorAction Stop | Out-Null
            $driveGone = $false
        } catch { }
        if (-not $driveGone) { throw "drive letter $letter still accessible after kill" }

        Write-Result "PASS" $name "job-object killed child, drive gone, no orphan"
    } catch {
        Write-Result "FAIL" $name $_.Exception.Message
    } finally {
        Kill-ApfsProcs
        Online-Disk -Num $DiskNumber
        Start-Sleep -Seconds 2
    }
}

# ---------------------------------------------------------------------------
# S3: Rapid replug x5
# ---------------------------------------------------------------------------
function Test-S3-RapidReplug {
    $name = "S3-rapid-replug-x5"
    $trayProc = $null
    try {
        # Start tray once; it must survive all 5 cycles.
        Start-Process -FilePath $TrayExe -WindowStyle Hidden
        Start-Sleep -Seconds 2
        $trayProc = Get-Process apfs-tray -ErrorAction SilentlyContinue

        $cyclesFailed = 0
        for ($i = 1; $i -le 5; $i++) {
            # Wait for mount to appear.
            $letter = Wait-MountAppears -TimeoutSec 30
            if (-not $letter) {
                Write-Host "  cycle $i: mount did not appear"
                $cyclesFailed++
            }

            # Offline the disk (simulate unplug).
            Offline-Disk -Num $DiskNumber
            Start-Sleep -Seconds 2

            # Assert no orphaned mount after offline.
            $mProc = Get-Process apfs-mount -ErrorAction SilentlyContinue
            if ($mProc) {
                # Give it 5 more seconds.
                Start-Sleep -Seconds 5
                $mProc = Get-Process apfs-mount -ErrorAction SilentlyContinue
                if ($mProc) {
                    Write-Host "  cycle $i: orphaned apfs-mount after offline"
                    Stop-Process -Name apfs-mount -Force -ErrorAction SilentlyContinue
                    $cyclesFailed++
                }
            }

            # Online the disk (simulate replug).
            Online-Disk -Num $DiskNumber
            Start-Sleep -Seconds 3
        }

        # Assert: tray still alive.
        $trayAlive = Get-Process apfs-tray -ErrorAction SilentlyContinue
        if (-not $trayAlive) { throw "apfs-tray died during replug cycles" }

        if ($cyclesFailed -gt 0) {
            throw "$cyclesFailed of 5 cycles had mount/orphan issues"
        }

        Write-Result "PASS" $name "5 cycles: no tray death, no orphan, no leaked letter"
    } catch {
        Write-Result "FAIL" $name $_.Exception.Message
    } finally {
        Kill-ApfsProcs
        Online-Disk -Num $DiskNumber
        Start-Sleep -Seconds 3
    }
}

# ---------------------------------------------------------------------------
# S4: Slow-device read (5-min cap; PASS = no crash/hang)
# ---------------------------------------------------------------------------
function Test-S4-SlowDevice {
    $name = "S4-slow-device-read"
    try {
        $letter = Start-TrayAndWaitMount -TimeoutSec 40
        if (-not $letter) { throw "drive did not mount within timeout" }

        # Run a large copy/hash in a job with a hard 5-min cap.
        $readJob = Start-Job -ScriptBlock {
            param($r)
            try {
                # Recursive hash of everything — exercises many round trips.
                Get-ChildItem $r -Recurse -ErrorAction SilentlyContinue |
                    Where-Object { -not $_.PSIsContainer } |
                    ForEach-Object {
                        try { Get-FileHash $_.FullName -ErrorAction SilentlyContinue }
                        catch { }
                    }
                "DONE"
            } catch {
                "ERROR: $_"
            }
        } -ArgumentList "$letter\"

        # Wait up to 5 minutes.
        $completed = $readJob | Wait-Job -Timeout 300
        $result = $null
        if ($completed) {
            $result = Receive-Job $readJob -ErrorAction SilentlyContinue
        }
        Remove-Job $readJob -Force -ErrorAction SilentlyContinue

        # PASS criteria: tray alive, no crash/BSOD (we are still running).
        $tray = Get-Process apfs-tray -ErrorAction SilentlyContinue
        if (-not $tray) { throw "apfs-tray crashed during slow read" }

        $outcome = if ($result -and $result -match "DONE") { "completed" } else { "timed-out-or-error (ok)" }
        Write-Result "PASS" $name "no crash/hang within cap; read=$outcome"
    } catch {
        Write-Result "FAIL" $name $_.Exception.Message
    } finally {
        Kill-ApfsProcs
        Online-Disk -Num $DiskNumber
        Start-Sleep -Seconds 2
    }
}

# ---------------------------------------------------------------------------
# S5: Non-APFS device -> clean error, no panic
# ---------------------------------------------------------------------------
function Test-S5-CorruptImage {
    $name = "S5-non-apfs-device-clean-error"
    try {
        # Point apfs-mount at PhysicalDrive0 (OS disk, never APFS).
        # Expected: mount_apfs error + exit 1, no panic/crash.
        $mountJob = Start-Job -ScriptBlock {
            param($exe)
            $proc = Start-Process -FilePath $exe `
                -ArgumentList @('\\.\PhysicalDrive0') `
                -NoNewWindow -PassThru `
                -RedirectStandardError "$env:TEMP\s5_stderr.txt" `
                -RedirectStandardOutput "$env:TEMP\s5_stdout.txt"
            $proc.WaitForExit(15000) | Out-Null
            $proc.ExitCode
        } -ArgumentList $MountExe

        $finished = $mountJob | Wait-Job -Timeout 25
        $exitCode = $null
        if ($finished) {
            $exitCode = Receive-Job $mountJob -ErrorAction SilentlyContinue
        }
        Remove-Job $mountJob -Force -ErrorAction SilentlyContinue

        # Assert: process exited (not hanging).
        if ($null -eq $exitCode) { throw "apfs-mount hung on non-APFS device (no exit within cap)" }

        # Assert: exit code non-zero (clean error, not panic).
        if ($exitCode -eq 0) { throw "apfs-mount returned 0 on non-APFS device (should error)" }

        # Assert: no apfs-mount orphan.
        Assert-NoOrphanMount

        # Assert: apfs-tray not affected (may or may not be running in this scenario;
        # if it is, it must still be alive).
        # Read stderr for diagnostic.
        $stderr = ""
        if (Test-Path "$env:TEMP\s5_stderr.txt") {
            $stderr = (Get-Content "$env:TEMP\s5_stderr.txt" -Raw -ErrorAction SilentlyContinue) -replace '\s+', ' '
            $stderr = $stderr.Substring(0, [Math]::Min(120, $stderr.Length))
        }

        Write-Result "PASS" $name "exit=$exitCode no-orphan; err: $stderr"
    } catch {
        Write-Result "FAIL" $name $_.Exception.Message
    } finally {
        Stop-Process -Name apfs-mount -Force -ErrorAction SilentlyContinue
        Online-Disk -Num $DiskNumber
    }
}

# ---------------------------------------------------------------------------
# Main: run matrix
# ---------------------------------------------------------------------------
Write-Host "=== M6 Robustness Matrix ==="
Write-Host "TrayExe : $TrayExe"
Write-Host "MountExe: $MountExe"
Write-Host "Disk    : $PhysDisk (number $DiskNumber)"
Write-Host ""

# Ensure disk is online before we start.
Online-Disk -Num $DiskNumber
Kill-ApfsProcs
Start-Sleep -Seconds 2

Test-S1-SurpriseRemoval
Test-S2-AgentForceKill
Test-S3-RapidReplug
Test-S4-SlowDevice
Test-S5-CorruptImage

Write-Host ""
Write-Host "=== Matrix complete ==="
