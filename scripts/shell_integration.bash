# mypixels shell integration (bash): command marks + a bell on failure.
#
#   source /path/to/this/file   (e.g. from ~/.bashrc)
#
# Emits OSC 133 marks - D (exit status of the last command) and A (new
# prompt) from PROMPT_COMMAND, B/C at command start via PS0 - which
# mypixels folds into per-tab command blocks for Ctrl+Alt+Up/Down command
# navigation. The failed-command BEL stays: mypixels escalates an unseen
# bell into a compositor attention request plus a red pulse on the tab's
# cell, and it works over ssh because it is all just bytes in the stream.
__mypixels_precmd() {
    local rc=$1
    printf '\e]133;D;%s\a' "$rc"
    if [ "$rc" -ne 0 ]; then
        printf '\a'
    fi
    printf '\e]133;A\a'
    return 0
}
__mypixels_preexec() {
    # PS0 fires after a line is read and before it runs: prompt over (B),
    # command starting (C). Bash's own $? is not yet meaningful here, which
    # is exactly why D belongs in PROMPT_COMMAND instead.
    printf '\e]133;B\a\e]133;C\a'
}
case ":$PROMPT_COMMAND:" in
    *:"__mypixels_precmd \$?"* | *:__mypixels_precmd*) ;;
    *) PROMPT_COMMAND="__mypixels_precmd \$?${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
esac
if [ -z "${PS0+x}" ] || [ "$PS0" = "__mypixels_preexec" ]; then
    PS0="__mypixels_preexec"
else
    PS0="$PS0;__mypixels_preexec"
fi
