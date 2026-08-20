# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_rclone_vfsmount_tray_global_optspecs
    string join \n log-level= h/help V/version
end

function __fish_rclone_vfsmount_tray_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_rclone_vfsmount_tray_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_rclone_vfsmount_tray_using_subcommand
    set -l cmd (__fish_rclone_vfsmount_tray_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_needs_command" -l log-level -d 'Log verbosity. Takes precedence over `RUST_LOG`; defaults to `info`' -r
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_needs_command" -s h -l help -d 'Print help'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_needs_command" -s V -l version -d 'Print version'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_needs_command" -f -a "list" -d 'List configured mounts and their state'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_needs_command" -f -a "mount" -d 'Mount one configured mount'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_needs_command" -f -a "unmount" -d 'Unmount one mount. Refused while anything is still using it unless `--force`'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_needs_command" -f -a "status" -d 'Print mount and transfer state'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_using_subcommand list" -s h -l help -d 'Print help'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_using_subcommand mount" -s h -l help -d 'Print help'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_using_subcommand unmount" -l force -d 'Unmount even while the mount is in use. A file being written is severed mid-write, and rclone later uploads the partial file as though complete'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_using_subcommand unmount" -s h -l help -d 'Print help'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_using_subcommand status" -l json -d 'Emit JSON. This is the stable surface for scripting and tests'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_using_subcommand status" -s h -l help -d 'Print help'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_using_subcommand help; and not __fish_seen_subcommand_from list mount unmount status help" -f -a "list" -d 'List configured mounts and their state'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_using_subcommand help; and not __fish_seen_subcommand_from list mount unmount status help" -f -a "mount" -d 'Mount one configured mount'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_using_subcommand help; and not __fish_seen_subcommand_from list mount unmount status help" -f -a "unmount" -d 'Unmount one mount. Refused while anything is still using it unless `--force`'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_using_subcommand help; and not __fish_seen_subcommand_from list mount unmount status help" -f -a "status" -d 'Print mount and transfer state'
complete -c rclone-vfsmount-tray -n "__fish_rclone_vfsmount_tray_using_subcommand help; and not __fish_seen_subcommand_from list mount unmount status help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
