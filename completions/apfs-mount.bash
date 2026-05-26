#!/usr/bin/env bash
# Bash completion for apfs-mount
# Install: source this file or place in /etc/bash_completion.d/apfs-mount

_apfs_mount() {
    local cur prev words cword
    _init_completion || return

    local subcommands="mount snapshot clone volume help"
    local global_opts="-h --help -V --version --log-level --log-file"

    # Determine the subcommand (first non-option argument after the command)
    local subcommand=""
    local i
    for (( i=1; i<cword; i++ )); do
        case "${words[$i]}" in
            mount|snapshot|clone|volume)
                subcommand="${words[$i]}"
                break
                ;;
        esac
    done

    case "$subcommand" in
        "")
            # Completing subcommand or global option
            case "$prev" in
                --log-level)
                    COMPREPLY=( $(compgen -W "error warn info debug trace" -- "$cur") )
                    return 0
                    ;;
                --log-file)
                    _filedir
                    return 0
                    ;;
            esac
            COMPREPLY=( $(compgen -W "$subcommands $global_opts" -- "$cur") )
            return 0
            ;;

        mount)
            case "$prev" in
                -d|--device)
                    _filedir
                    return 0
                    ;;
                -v|--volume)
                    # Volume name completion — dynamic; return empty if no device known
                    return 0
                    ;;
                -l|--letter)
                    COMPREPLY=( $(compgen -W "D E F G H I J K L M N O P Q R S T U V W X Y Z" -- "$cur") )
                    return 0
                    ;;
                --log-level)
                    COMPREPLY=( $(compgen -W "error warn info debug trace" -- "$cur") )
                    return 0
                    ;;
                --log-file)
                    _filedir
                    return 0
                    ;;
            esac
            local mount_opts="-d --device -v --volume -l --letter -r --read-only --uid --gid --log-level --log-file -h --help"
            COMPREPLY=( $(compgen -W "$mount_opts" -- "$cur") )
            return 0
            ;;

        snapshot)
            # Check if snapshot sub-subcommand is already present
            local snap_sub=""
            for (( i=2; i<cword; i++ )); do
                case "${words[$i]}" in
                    list|create|delete|revert)
                        snap_sub="${words[$i]}"
                        break
                        ;;
                esac
            done

            if [[ -z "$snap_sub" ]]; then
                COMPREPLY=( $(compgen -W "list create delete revert" -- "$cur") )
                return 0
            fi

            case "$prev" in
                -d|--device)
                    _filedir
                    return 0
                    ;;
                -v|--volume|-n|--name)
                    return 0
                    ;;
                --log-level)
                    COMPREPLY=( $(compgen -W "error warn info debug trace" -- "$cur") )
                    return 0
                    ;;
                --log-file)
                    _filedir
                    return 0
                    ;;
            esac
            local snap_opts="-d --device -v --volume -n --name --log-level --log-file -h --help"
            COMPREPLY=( $(compgen -W "$snap_opts" -- "$cur") )
            return 0
            ;;

        clone)
            case "$prev" in
                -d|--device)
                    _filedir
                    return 0
                    ;;
                -v|--volume|--src|--dst)
                    return 0
                    ;;
                --log-level)
                    COMPREPLY=( $(compgen -W "error warn info debug trace" -- "$cur") )
                    return 0
                    ;;
                --log-file)
                    _filedir
                    return 0
                    ;;
            esac
            local clone_opts="-d --device -v --volume --src --dst --log-level --log-file -h --help"
            COMPREPLY=( $(compgen -W "$clone_opts" -- "$cur") )
            return 0
            ;;

        volume)
            local vol_sub=""
            for (( i=2; i<cword; i++ )); do
                case "${words[$i]}" in
                    list|info)
                        vol_sub="${words[$i]}"
                        break
                        ;;
                esac
            done

            if [[ -z "$vol_sub" ]]; then
                COMPREPLY=( $(compgen -W "list info" -- "$cur") )
                return 0
            fi

            case "$prev" in
                -d|--device)
                    _filedir
                    return 0
                    ;;
                -v|--volume)
                    return 0
                    ;;
                --log-level)
                    COMPREPLY=( $(compgen -W "error warn info debug trace" -- "$cur") )
                    return 0
                    ;;
                --log-file)
                    _filedir
                    return 0
                    ;;
            esac
            local vol_opts="-d --device -v --volume --log-level --log-file -h --help"
            COMPREPLY=( $(compgen -W "$vol_opts" -- "$cur") )
            return 0
            ;;
    esac
}

complete -F _apfs_mount apfs-mount
