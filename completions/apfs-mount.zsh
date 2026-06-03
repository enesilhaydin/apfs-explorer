#compdef apfs-mount
# Zsh completion for apfs-mount
# Install: place in a directory on $fpath, or add to ~/.zshrc:
#   fpath=(/path/to/completions $fpath)

_apfs_mount() {
    local state line
    typeset -A opt_args

    _arguments -C \
        '(-h --help)'{-h,--help}'[show help and exit]' \
        '(-V --version)'{-V,--version}'[show version and exit]' \
        '--log-level=[set log verbosity]:level:(error warn info debug trace)' \
        '--log-file=[write log to file]:file:_files' \
        '1:subcommand:->subcommand' \
        '*::args:->args'

    case $state in
        subcommand)
            local subcommands
            subcommands=(
                'mount:mount an APFS volume as a Windows drive letter'
                'snapshot:manage APFS snapshots'
                'clone:copy-on-write file clone within a volume'
                'volume:list or inspect APFS volumes'
                'help:show help for a subcommand'
            )
            _describe 'subcommand' subcommands
            ;;

        args)
            case $line[1] in
                mount)
                    _arguments \
                        '(-d --device)'{-d,--device}'[block device or image file]:device:_files' \
                        '(-v --volume)'{-v,--volume}'[APFS volume name]:volume' \
                        '(-l --letter)'{-l,--letter}'[drive letter to assign]:letter:(D E F G H I J K L M N O P Q R S T U V W X Y Z)' \
                        '(-r --read-only)'{-r,--read-only}'[mount read-only]' \
                        '--uid=[override UID for all files]:uid' \
                        '--gid=[override GID for all files]:gid' \
                        '--log-level=[set log verbosity]:level:(error warn info debug trace)' \
                        '--log-file=[write log to file]:file:_files' \
                        '(-h --help)'{-h,--help}'[show help]'
                    ;;

                snapshot)
                    _arguments -C \
                        '1:snapshot-action:->snap_action' \
                        '*::snap-args:->snap_args'

                    case $state in
                        snap_action)
                            local snap_actions
                            snap_actions=(
                                'list:list all snapshots'
                                'create:create a new snapshot'
                                'delete:delete a snapshot'
                                'revert:revert volume to a snapshot'
                            )
                            _describe 'snapshot action' snap_actions
                            ;;
                        snap_args)
                            _arguments \
                                '(-d --device)'{-d,--device}'[block device or image file]:device:_files' \
                                '(-v --volume)'{-v,--volume}'[APFS volume name]:volume' \
                                '(-n --name)'{-n,--name}'[snapshot name]:name' \
                                '--log-level=[set log verbosity]:level:(error warn info debug trace)' \
                                '--log-file=[write log to file]:file:_files' \
                                '(-h --help)'{-h,--help}'[show help]'
                            ;;
                    esac
                    ;;

                clone)
                    _arguments \
                        '(-d --device)'{-d,--device}'[block device or image file]:device:_files' \
                        '(-v --volume)'{-v,--volume}'[APFS volume name]:volume' \
                        '--src=[source file path (APFS absolute path)]:src path' \
                        '--dst=[destination file path (APFS absolute path)]:dst path' \
                        '--log-level=[set log verbosity]:level:(error warn info debug trace)' \
                        '--log-file=[write log to file]:file:_files' \
                        '(-h --help)'{-h,--help}'[show help]'
                    ;;

                volume)
                    _arguments -C \
                        '1:volume-action:->vol_action' \
                        '*::vol-args:->vol_args'

                    case $state in
                        vol_action)
                            local vol_actions
                            vol_actions=(
                                'list:list all volumes in the container'
                                'info:show detailed info about a volume'
                            )
                            _describe 'volume action' vol_actions
                            ;;
                        vol_args)
                            _arguments \
                                '(-d --device)'{-d,--device}'[block device or image file]:device:_files' \
                                '(-v --volume)'{-v,--volume}'[APFS volume name]:volume' \
                                '--log-level=[set log verbosity]:level:(error warn info debug trace)' \
                                '--log-file=[write log to file]:file:_files' \
                                '(-h --help)'{-h,--help}'[show help]'
                            ;;
                    esac
                    ;;
            esac
            ;;
    esac
}

_apfs_mount "$@"
