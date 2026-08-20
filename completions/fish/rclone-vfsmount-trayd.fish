# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_rclone_vfsmount_trayd_global_optspecs
    string join \n config= log-level= foreground h/help V/version
end

function __fish_rclone_vfsmount_trayd_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_rclone_vfsmount_trayd_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_rclone_vfsmount_trayd_using_subcommand
    set -l cmd (__fish_rclone_vfsmount_trayd_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c rclone-vfsmount-trayd -n "__fish_rclone_vfsmount_trayd_needs_command" -l config -d 'Path to the configuration file. Defaults to `$XDG_CONFIG_HOME/rclone-vfsmount-tray/config.toml`' -r -F
complete -c rclone-vfsmount-trayd -n "__fish_rclone_vfsmount_trayd_needs_command" -l log-level -d 'Log verbosity. Takes precedence over `RUST_LOG`; defaults to `info`' -r
complete -c rclone-vfsmount-trayd -n "__fish_rclone_vfsmount_trayd_needs_command" -l foreground -d 'Accepted and ignored. It will mean "stay in the foreground and log to stderr" once there is a background mode to opt out of; today that is all the service does'
complete -c rclone-vfsmount-trayd -n "__fish_rclone_vfsmount_trayd_needs_command" -s h -l help -d 'Print help'
complete -c rclone-vfsmount-trayd -n "__fish_rclone_vfsmount_trayd_needs_command" -s V -l version -d 'Print version'
complete -c rclone-vfsmount-trayd -n "__fish_rclone_vfsmount_trayd_needs_command" -f -a "prepare-mount" -d 'Clear what a hard-killed rclone left behind, so its unit can start'
complete -c rclone-vfsmount-trayd -n "__fish_rclone_vfsmount_trayd_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c rclone-vfsmount-trayd -n "__fish_rclone_vfsmount_trayd_using_subcommand prepare-mount" -l name -r
complete -c rclone-vfsmount-trayd -n "__fish_rclone_vfsmount_trayd_using_subcommand prepare-mount" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c rclone-vfsmount-trayd -n "__fish_rclone_vfsmount_trayd_using_subcommand help; and not __fish_seen_subcommand_from prepare-mount help" -f -a "prepare-mount" -d 'Clear what a hard-killed rclone left behind, so its unit can start'
complete -c rclone-vfsmount-trayd -n "__fish_rclone_vfsmount_trayd_using_subcommand help; and not __fish_seen_subcommand_from prepare-mount help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
