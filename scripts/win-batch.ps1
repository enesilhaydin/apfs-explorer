# -Drive N runs the batch on \\.\PhysicalDriveN (a real drive authored the
# same way) instead of the image.
param([string]$Img = "C:\apfs\audit\win-batch.img", [string]$Letter = "U:", [string]$Out = "C:\apfs\audit\win-batch.after",
      [string]$Exe = ".\target\debug\apfs-mount.exe", [string]$Drive = "")
$ErrorActionPreference = "Continue"
Set-Location C:\apfs\audit
$target = if ($Drive -ne "") { @("--drive", $Drive) } else { @("--image", $Img) }
$p = Start-Process -FilePath $Exe `
    -ArgumentList ($target + @("--mount",$Letter,"--writable","--wait-for-kill")) `
    -PassThru -WindowStyle Hidden
for ($i = 0; $i -lt 40; $i++) { if (Test-Path "$Letter\") { break }; Start-Sleep -Milliseconds 250 }
if (-not (Test-Path "$Letter\")) { Write-Output "MOUNT-FAILED"; $p | Stop-Process -Force; exit 1 }
Write-Output "MOUNTED"

function Step($name, $block) {
    try { & $block; Write-Output ("OK " + $name) }
    catch { Write-Output ("FAIL " + $name + ": " + $_.Exception.GetType().Name + " " + $_.Exception.Message) }
}

# (a) SetEndOfFile past EOF, and a cut inside a block.
Step "grow plain.bin past EOF" { $fs = [System.IO.File]::Open("$Letter\plain.bin", 'Open', 'ReadWrite'); $fs.SetLength(307200); $fs.Close() }
Step "cut plain2.bin inside a block" { $fs = [System.IO.File]::Open("$Letter\plain2.bin", 'Open', 'ReadWrite'); $fs.SetLength(153723); $fs.Close() }
Step "grow sparse.bin further" { $fs = [System.IO.File]::Open("$Letter\sparse.bin", 'Open', 'ReadWrite'); $fs.SetLength(200000); $fs.Close() }

# (b) Files macOS stored compressed: resize, write into, append, delete.
Step "grow compressed.bin" { $fs = [System.IO.File]::Open("$Letter\compressed.bin", 'Open', 'ReadWrite'); $fs.SetLength(200000); $fs.Close() }
Step "cut m04_zlib_rsrc_big.bin" { $fs = [System.IO.File]::Open("$Letter\cmp\m04_zlib_rsrc_big.bin", 'Open', 'ReadWrite'); $fs.SetLength(70001); $fs.Close() }
Step "write into m12_lzfse_rsrc_big.bin" {
    $bytes = New-Object byte[] 5000
    for ($i = 0; $i -lt 5000; $i++) { $bytes[$i] = [byte](($i * 3) % 251) }
    $fs = [System.IO.File]::Open("$Letter\cmp\m12_lzfse_rsrc_big.bin", 'Open', 'ReadWrite')
    $fs.Seek(1000, 'Begin') | Out-Null; $fs.Write($bytes, 0, 5000); $fs.Close()
}
Step "append to m14_lzbitmap_rsrc_text.bin" { [System.IO.File]::AppendAllText("$Letter\cmp\m14_lzbitmap_rsrc_text.bin", "appended on windows") }
Step "overwrite m13_lzbitmap_attr_text.bin" { [System.IO.File]::WriteAllText("$Letter\cmp\m13_lzbitmap_attr_text.bin", "replaced on windows") }
Step "delete m07_lzvn_attr_random.bin" { Remove-Item "$Letter\cmp\m07_lzvn_attr_random.bin" }
Step "rename over m10 with m09" { Move-Item -Force "$Letter\cmp\m09_plain_attr_text.bin" "$Letter\cmp\m10_plain_rsrc_text.bin" }
Step "copy a compressed file" { Copy-Item "$Letter\cmp\m08_lzvn_rsrc_text.bin" "$Letter\cmp\lzvn_copy.bin" }

# (c) Clones and hard links: delete, overwrite, rename over.
Step "delete one side of a clone" { Remove-Item "$Letter\clone_a.bin" }
Step "overwrite one side of a clone" {
    $bytes = New-Object byte[] 70000
    for ($i = 0; $i -lt 70000; $i++) { $bytes[$i] = [byte](($i * 11) % 251) }
    [System.IO.File]::WriteAllBytes("$Letter\clone2_a.bin", $bytes)
}
Step "rename over the other side of a clone" { Move-Item -Force "$Letter\other.txt" "$Letter\clone2_b.bin" }
Step "delete the primary name of a hard link" { Remove-Item "$Letter\link_a.txt" }
Step "rename over one name of a hard link" { Move-Item -Force "$Letter\other2.txt" "$Letter\sub\link2_b.txt" }
Step "overwrite through a hard link" { [System.IO.File]::WriteAllText("$Letter\link2_a.txt", "through the link") }
Step "a handle reads back what it wrote" {
    [System.IO.File]::WriteAllText("$Letter\readback.txt", "old content")
    $fs = [System.IO.File]::Open("$Letter\readback.txt", 'Open', 'ReadWrite')
    $fs.Write([System.Text.Encoding]::ASCII.GetBytes("NEW"), 0, 3)
    $fs.Seek(0, 'Begin') | Out-Null
    $r = New-Object byte[] 64
    $n = $fs.Read($r, 0, 64)
    $fs.Close()
    $got = [System.Text.Encoding]::ASCII.GetString($r, 0, $n)
    if ($got -ne "NEW content") { throw ("read back '" + $got + "'") }
}

# (d) Timestamps through the host, as macOS keeps them: a move leaves the
#     file's modification time alone, and a modification time set earlier than
#     the birth time pulls the birth time back to it.
Step "a move keeps the modification time" {
    [System.IO.File]::WriteAllText("$Letter\times_move.txt", "moved between directories")
    $before = (Get-Item "$Letter\times_move.txt").LastWriteTimeUtc
    Start-Sleep -Milliseconds 1200
    Move-Item "$Letter\times_move.txt" "$Letter\sub\times_move.txt"
    $after = (Get-Item "$Letter\sub\times_move.txt").LastWriteTimeUtc
    if ($after -ne $before) { throw ("modification time moved: " + $before.ToString("o") + " -> " + $after.ToString("o")) }
}
Step "an earlier modification time pulls the birth time back" {
    [System.IO.File]::WriteAllText("$Letter\times_birth.txt", "dated back")
    $past = New-Object DateTime 2020, 1, 1, 0, 0, 0, ([DateTimeKind]::Utc)
    (Get-Item "$Letter\times_birth.txt").LastWriteTimeUtc = $past
    $item = Get-Item "$Letter\times_birth.txt"
    if ($item.LastWriteTimeUtc -ne $past) { throw ("modification time not set: " + $item.LastWriteTimeUtc.ToString("o")) }
    if ($item.CreationTimeUtc -ne $past) { throw ("birth time " + $item.CreationTimeUtc.ToString("o") + " did not follow " + $past.ToString("o")) }
}

# (e) A directory needing more than one ReadDirectory buffer lists in full.
Step "a directory of 1500 files lists in full" {
    New-Item -ItemType Directory "$Letter\many" | Out-Null
    for ($i = 0; $i -lt 1500; $i++) { [System.IO.File]::WriteAllText("$Letter\many\entry$i.txt", "$i") }
    $n = @(Get-ChildItem "$Letter\many").Count
    if ($n -ne 1500) { throw ("listed " + $n + " of 1500") }
}

# What Windows sees afterwards: size and hash of every file.
$lines = @()
# Symlinks and hidden folders (.fseventsd on a drive macOS mounted) are left
# out on both sides.
$listed = Get-ChildItem "$Letter\" -Recurse -File |
    Where-Object { -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -and $_.FullName -notmatch '\\\.' } |
    Sort-Object FullName
foreach ($f in $listed) {
    $rel = $f.FullName.Substring($Letter.Length + 1).Replace("\", "/")
    try { $h = (Get-FileHash -Algorithm SHA256 -Path $f.FullName).Hash.ToLower() } catch { $h = "ERROR" }
    $lines += ($h + "  " + $f.Length + "  " + $rel)
}
[System.IO.File]::WriteAllLines($Out, $lines)
Write-Output ("FILES: " + $lines.Count)
Start-Sleep -Seconds 1
$p | Stop-Process -Force
Start-Sleep -Seconds 2
Write-Output "UNMOUNTED"
