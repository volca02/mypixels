# mypixels shell integration (fish): a failed command rings the bell.
#
#   source /path/to/this/file   (e.g. from ~/.config/fish/config.fish)
#
# See shell_integration.bash for the meaning: mypixels escalates a BEL that
# arrives unseen into a compositor attention request + a red tab pulse.
function __mypixels_postexec --on-event fish_postexec
    if test $status -gt 0
        printf '\a'
    end
end
