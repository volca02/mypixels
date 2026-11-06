# mypixels shell integration (fish): command marks + a bell on failure.
#
#   source /path/to/this/file   (e.g. from ~/.config/fish/config.fish)
#
# Source after prompt setup. Lifecycle events emit A/C/D, and a prompt
# wrapper emits B after the user's prompt has been rendered.
# See shell_integration.bash for the bell.
# fish 4 emits A/C/D itself. Do not duplicate those markers: a second D
# would close the next pending prompt and create a spurious history row.
set -g __mypixels_native_marks 0
status test-feature mark-prompt
switch $status
    case 0
        set -g __mypixels_native_marks 1
    case 2
        # Early fish 4 releases had native markers before the feature flag.
        if test (string split . $version)[1] -ge 4
            set -g __mypixels_native_marks 1
        end
end

# Erase previous event registrations before redefining functions on reload.
functions --erase __mypixels_postexec __mypixels_preexec __mypixels_prompt
function __mypixels_postexec --on-event fish_postexec
    set -l rc $status
    if test $__mypixels_native_marks -eq 0
        printf '\e]133;D;%d\a' $rc
    end
    if test $rc -gt 0
        printf '\a'
    end
end
function __mypixels_preexec --on-event fish_preexec
    if test $__mypixels_native_marks -eq 0
        printf '\e]133;C\a'
    end
end
function __mypixels_prompt --on-event fish_prompt
    if test $__mypixels_native_marks -eq 0
        printf '\e]133;A\a'
    end
end

# Keep the user's prompt intact and mark its END, not the preexec row.
if not functions -q __mypixels_user_prompt
    functions --copy fish_prompt __mypixels_user_prompt
    function fish_prompt
        __mypixels_user_prompt
        printf '\e]133;B\a'
    end
end
