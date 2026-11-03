# mypixels shell integration (bash): a failed command rings the bell.
#
#   source /path/to/this/file   (e.g. from ~/.bashrc)
#
# When the window is not focused - or the bell came from a tab you are not
# looking at - mypixels turns that BEL into a compositor attention request
# and a red pulse on the tab's cell. That is "tell me when the build breaks",
# with no daemon, no wrapper command and no OSC 133 required: the bell is
# the oldest status channel in the terminal and the one every shell can
# reach, even over ssh.
__mypixels_precmd() {
    if [ "${1:-0}" -ne 0 ]; then
        printf '\a'
    fi
    return 0
}
case ":$PROMPT_COMMAND:" in
    *:"__mypixels_precmd \$?"* | *:__mypixels_precmd*) ;;
    *) PROMPT_COMMAND="__mypixels_precmd \$?${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
esac
