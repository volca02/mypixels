# mypixels shell integration (fish): command marks + a bell on failure.
#
#   source /path/to/this/file   (e.g. from ~/.config/fish/config.fish)
#
# OSC 133 via fish's own lifecycle events - no prompt wrapping needed:
# fish_postexec reports D with the status, fish_preexec reports C, and
# fish_prompt opens the next A. See shell_integration.bash for the bell.
function __mypixels_postexec --on-event fish_postexec
    printf '\e]133;D;%d\a' $status
    if test $status -gt 0
        printf '\a'
    end
end
function __mypixels_preexec --on-event fish_preexec
    printf '\e]133;B\a\e]133;C\a'
end
function __mypixels_prompt --on-event fish_prompt
    printf '\e]133;A\a'
end
