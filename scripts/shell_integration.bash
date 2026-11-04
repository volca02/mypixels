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
    # is exactly why D belongs in PROMPT_COMMAND instead. The write goes to
    # the tty, not stdout: PS0 runs this inside a command substitution, and
    # stdout bytes emitted there are captured as the expansion's value and
    # thrown away, never reaching the terminal.
    printf '\e]133;B\a\e]133;C\a' > /dev/tty
}
case ":$PROMPT_COMMAND:" in
    *:"__mypixels_precmd \$?"* | *:__mypixels_precmd*) ;;
    *) PROMPT_COMMAND="__mypixels_precmd \$?${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
esac
# PS0 undergoes PROMPT-LIKE expansion, then the RESULT is executed as a
# command: a bare function name just prints itself on the command line
# and runs nothing. The call must ride inside $( ) substitution (whose
# side effects are exactly what we want, discarding its empty stdout).
if [ -z "${PS0+x}" ] || [ "$PS0" = "\$(__mypixels_preexec)" ]; then
    PS0='$(__mypixels_preexec)'
else
    PS0="$PS0;__mypixels_preexec"
fi
