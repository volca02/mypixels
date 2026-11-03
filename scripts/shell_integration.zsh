# mypixels shell integration (zsh): a failed command rings the bell.
#
#   source /path/to/this/file   (e.g. from ~/.zshrc)
#
# See shell_integration.bash for why the bell is the whole feature: mypixels
# escalates an unseen BEL (unfocused window, or a tab you are not looking at)
# into a compositor attention request plus a red pulse on that tab's cell.
# Works across ssh too - the escape travels in the stream.
__mypixels_precmd() {
    if (( $? )); then
        printf '\a'
    fi
}
autoload -U add-zsh-hook
add-zsh-hook precmd __mypixels_precmd
