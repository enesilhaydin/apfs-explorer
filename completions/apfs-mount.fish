# Fish completion for apfs-mount
# Install: place in ~/.config/fish/completions/ or /usr/share/fish/vendor_completions.d/

# Disable file completion by default
complete -c apfs-mount -f

# Global options
complete -c apfs-mount -s h -l help    -d 'Show help and exit'
complete -c apfs-mount -s V -l version -d 'Show version and exit'
complete -c apfs-mount -l log-level    -d 'Set log verbosity' -r \
    -a 'error\t"errors only" warn\t"warnings and errors" info\t"informational (default)" debug\t"debug output" trace\t"full trace"'
complete -c apfs-mount -l log-file     -d 'Write log to file' -r -F

# ---------------------------------------------------------------------------
# Subcommands
# ---------------------------------------------------------------------------

complete -c apfs-mount -n '__fish_use_subcommand' -a mount    -d 'Mount an APFS volume as a Windows drive letter'
complete -c apfs-mount -n '__fish_use_subcommand' -a snapshot -d 'Manage APFS snapshots'
complete -c apfs-mount -n '__fish_use_subcommand' -a clone    -d 'Copy-on-write file clone within a volume'
complete -c apfs-mount -n '__fish_use_subcommand' -a volume   -d 'List or inspect APFS volumes'
complete -c apfs-mount -n '__fish_use_subcommand' -a help     -d 'Show help for a subcommand'

# ---------------------------------------------------------------------------
# mount subcommand
# ---------------------------------------------------------------------------

complete -c apfs-mount -n '__fish_seen_subcommand_from mount' \
    -s d -l device -d 'Block device or image file' -r -F
complete -c apfs-mount -n '__fish_seen_subcommand_from mount' \
    -s v -l volume -d 'APFS volume name' -r
complete -c apfs-mount -n '__fish_seen_subcommand_from mount' \
    -s l -l letter -d 'Drive letter to assign' -r \
    -a 'D E F G H I J K L M N O P Q R S T U V W X Y Z'
complete -c apfs-mount -n '__fish_seen_subcommand_from mount' \
    -s r -l read-only -d 'Mount in read-only mode'
complete -c apfs-mount -n '__fish_seen_subcommand_from mount' \
    -l uid -d 'Override UID for all files' -r
complete -c apfs-mount -n '__fish_seen_subcommand_from mount' \
    -l gid -d 'Override GID for all files' -r
complete -c apfs-mount -n '__fish_seen_subcommand_from mount' \
    -l log-level -d 'Set log verbosity' -r \
    -a 'error warn info debug trace'
complete -c apfs-mount -n '__fish_seen_subcommand_from mount' \
    -l log-file -d 'Write log to file' -r -F
complete -c apfs-mount -n '__fish_seen_subcommand_from mount' \
    -s h -l help -d 'Show help'

# ---------------------------------------------------------------------------
# snapshot subcommand
# ---------------------------------------------------------------------------

complete -c apfs-mount -n '__fish_seen_subcommand_from snapshot; and not __fish_seen_subcommand_from list create delete revert' \
    -a list   -d 'List all snapshots'
complete -c apfs-mount -n '__fish_seen_subcommand_from snapshot; and not __fish_seen_subcommand_from list create delete revert' \
    -a create -d 'Create a new snapshot'
complete -c apfs-mount -n '__fish_seen_subcommand_from snapshot; and not __fish_seen_subcommand_from list create delete revert' \
    -a delete -d 'Delete a snapshot'
complete -c apfs-mount -n '__fish_seen_subcommand_from snapshot; and not __fish_seen_subcommand_from list create delete revert' \
    -a revert -d 'Revert volume to a snapshot (destructive)'

complete -c apfs-mount -n '__fish_seen_subcommand_from snapshot' \
    -s d -l device -d 'Block device or image file' -r -F
complete -c apfs-mount -n '__fish_seen_subcommand_from snapshot' \
    -s v -l volume -d 'APFS volume name' -r
complete -c apfs-mount -n '__fish_seen_subcommand_from snapshot' \
    -s n -l name -d 'Snapshot name' -r
complete -c apfs-mount -n '__fish_seen_subcommand_from snapshot' \
    -l log-level -d 'Set log verbosity' -r \
    -a 'error warn info debug trace'
complete -c apfs-mount -n '__fish_seen_subcommand_from snapshot' \
    -l log-file -d 'Write log to file' -r -F
complete -c apfs-mount -n '__fish_seen_subcommand_from snapshot' \
    -s h -l help -d 'Show help'

# ---------------------------------------------------------------------------
# clone subcommand
# ---------------------------------------------------------------------------

complete -c apfs-mount -n '__fish_seen_subcommand_from clone' \
    -s d -l device -d 'Block device or image file' -r -F
complete -c apfs-mount -n '__fish_seen_subcommand_from clone' \
    -s v -l volume -d 'APFS volume name' -r
complete -c apfs-mount -n '__fish_seen_subcommand_from clone' \
    -l src -d 'Source file path (APFS absolute path)' -r
complete -c apfs-mount -n '__fish_seen_subcommand_from clone' \
    -l dst -d 'Destination file path (APFS absolute path)' -r
complete -c apfs-mount -n '__fish_seen_subcommand_from clone' \
    -l log-level -d 'Set log verbosity' -r \
    -a 'error warn info debug trace'
complete -c apfs-mount -n '__fish_seen_subcommand_from clone' \
    -l log-file -d 'Write log to file' -r -F
complete -c apfs-mount -n '__fish_seen_subcommand_from clone' \
    -s h -l help -d 'Show help'

# ---------------------------------------------------------------------------
# volume subcommand
# ---------------------------------------------------------------------------

complete -c apfs-mount -n '__fish_seen_subcommand_from volume; and not __fish_seen_subcommand_from list info' \
    -a list -d 'List all volumes in the container'
complete -c apfs-mount -n '__fish_seen_subcommand_from volume; and not __fish_seen_subcommand_from list info' \
    -a info -d 'Show detailed info about a volume'

complete -c apfs-mount -n '__fish_seen_subcommand_from volume' \
    -s d -l device -d 'Block device or image file' -r -F
complete -c apfs-mount -n '__fish_seen_subcommand_from volume' \
    -s v -l volume -d 'APFS volume name' -r
complete -c apfs-mount -n '__fish_seen_subcommand_from volume' \
    -l log-level -d 'Set log verbosity' -r \
    -a 'error warn info debug trace'
complete -c apfs-mount -n '__fish_seen_subcommand_from volume' \
    -l log-file -d 'Write log to file' -r -F
complete -c apfs-mount -n '__fish_seen_subcommand_from volume' \
    -s h -l help -d 'Show help'
