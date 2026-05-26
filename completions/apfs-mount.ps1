# PowerShell tab completion for apfs-mount
# Install: add to your $PROFILE:
#   . C:\path\to\apfs-mount.ps1

Register-ArgumentCompleter -Native -CommandName apfs-mount -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    # Parse the command AST to find the current subcommand
    $elements = $commandAst.CommandElements
    $subcommand = $null
    $snapshotAction = $null
    $volumeAction = $null

    for ($i = 1; $i -lt $elements.Count; $i++) {
        $el = $elements[$i].ToString()
        if ($el -notmatch '^-') {
            if ($null -eq $subcommand) {
                $subcommand = $el
            } elseif ($subcommand -eq 'snapshot' -and $null -eq $snapshotAction) {
                $snapshotAction = $el
            } elseif ($subcommand -eq 'volume' -and $null -eq $volumeAction) {
                $volumeAction = $el
            }
            break
        }
    }

    # Helper: emit a completion result
    function Complete {
        param([string]$text, [string]$tooltip = '')
        [System.Management.Automation.CompletionResult]::new(
            $text, $text,
            [System.Management.Automation.CompletionResultType]::ParameterValue,
            $(if ($tooltip) { $tooltip } else { $text })
        )
    }

    # Helper: filter by prefix
    function Filter {
        param([array]$items, [string]$prefix)
        $items | Where-Object { $_ -like "$prefix*" }
    }

    $logLevels = @('error', 'warn', 'info', 'debug', 'trace')
    $driveLetters = 'D','E','F','G','H','I','J','K','L','M','N','O','P','Q','R','S','T','U','V','W','X','Y','Z'

    # Completing a flag value
    $prevElement = if ($cursorPosition -gt 0 -and $elements.Count -gt 1) {
        $elements[$elements.Count - 2].ToString()
    } else { '' }

    switch ($prevElement) {
        { $_ -in '--log-level' } {
            Filter $logLevels $wordToComplete | ForEach-Object { Complete $_ "Log verbosity level" }
            return
        }
        { $_ -in '--log-file' } {
            # File path completion
            Get-ChildItem -Path "$wordToComplete*" -ErrorAction SilentlyContinue |
                ForEach-Object { Complete $_.FullName "File path" }
            return
        }
        { $_ -in '-d', '--device' } {
            Get-ChildItem -Path "$wordToComplete*" -ErrorAction SilentlyContinue |
                ForEach-Object { Complete $_.FullName "Device or image file" }
            return
        }
        { $_ -in '-l', '--letter' } {
            Filter $driveLetters $wordToComplete | ForEach-Object { Complete $_ "Drive letter" }
            return
        }
        { $_ -in '-v', '--volume', '-n', '--name', '--src', '--dst', '--uid', '--gid' } {
            # No static completions available for these (dynamic / user-supplied values)
            return
        }
    }

    # Top-level: subcommand or global option
    if ($null -eq $subcommand) {
        $items = @(
            'mount|Mount an APFS volume as a Windows drive letter',
            'snapshot|Manage APFS snapshots',
            'clone|Copy-on-write file clone within a volume',
            'volume|List or inspect APFS volumes',
            'help|Show help for a subcommand',
            '--help|Show help and exit',
            '--version|Show version and exit',
            '--log-level|Set log verbosity',
            '--log-file|Write log to file'
        )
        $items | ForEach-Object {
            $parts = $_ -split '\|', 2
            if ($parts[0] -like "$wordToComplete*") {
                Complete $parts[0] $parts[1]
            }
        }
        return
    }

    switch ($subcommand) {
        'mount' {
            $opts = @(
                '-d|Block device or image file', '--device|Block device or image file',
                '-v|APFS volume name', '--volume|APFS volume name',
                '-l|Drive letter', '--letter|Drive letter',
                '-r|Mount read-only', '--read-only|Mount read-only',
                '--uid|Override UID', '--gid|Override GID',
                '--log-level|Set log verbosity', '--log-file|Write log to file',
                '-h|Show help', '--help|Show help'
            )
            $opts | ForEach-Object {
                $parts = $_ -split '\|', 2
                if ($parts[0] -like "$wordToComplete*") { Complete $parts[0] $parts[1] }
            }
        }
        'snapshot' {
            if ($null -eq $snapshotAction) {
                $actions = @(
                    'list|List all snapshots',
                    'create|Create a new snapshot',
                    'delete|Delete a snapshot',
                    'revert|Revert volume to a snapshot (destructive)'
                )
                $actions | ForEach-Object {
                    $parts = $_ -split '\|', 2
                    if ($parts[0] -like "$wordToComplete*") { Complete $parts[0] $parts[1] }
                }
            } else {
                $opts = @(
                    '-d|Block device or image file', '--device|Block device or image file',
                    '-v|APFS volume name', '--volume|APFS volume name',
                    '-n|Snapshot name', '--name|Snapshot name',
                    '--log-level|Set log verbosity', '--log-file|Write log to file',
                    '-h|Show help', '--help|Show help'
                )
                $opts | ForEach-Object {
                    $parts = $_ -split '\|', 2
                    if ($parts[0] -like "$wordToComplete*") { Complete $parts[0] $parts[1] }
                }
            }
        }
        'clone' {
            $opts = @(
                '-d|Block device or image file', '--device|Block device or image file',
                '-v|APFS volume name', '--volume|APFS volume name',
                '--src|Source file path', '--dst|Destination file path',
                '--log-level|Set log verbosity', '--log-file|Write log to file',
                '-h|Show help', '--help|Show help'
            )
            $opts | ForEach-Object {
                $parts = $_ -split '\|', 2
                if ($parts[0] -like "$wordToComplete*") { Complete $parts[0] $parts[1] }
            }
        }
        'volume' {
            if ($null -eq $volumeAction) {
                $actions = @(
                    'list|List all volumes in the container',
                    'info|Show detailed info about a volume'
                )
                $actions | ForEach-Object {
                    $parts = $_ -split '\|', 2
                    if ($parts[0] -like "$wordToComplete*") { Complete $parts[0] $parts[1] }
                }
            } else {
                $opts = @(
                    '-d|Block device or image file', '--device|Block device or image file',
                    '-v|APFS volume name', '--volume|APFS volume name',
                    '--log-level|Set log verbosity', '--log-file|Write log to file',
                    '-h|Show help', '--help|Show help'
                )
                $opts | ForEach-Object {
                    $parts = $_ -split '\|', 2
                    if ($parts[0] -like "$wordToComplete*") { Complete $parts[0] $parts[1] }
                }
            }
        }
    }
}
