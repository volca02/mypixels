# mypixels shell integration (zsh): command marks + a bell on failure.
# Source from ~/.zshrc, after prompt setup. Sourcing twice is harmless.
# B belongs at the end of the rendered prompt, before the user types;
# preexec runs after ZLE has echoed the input and moved to the next row.
__mypixels_precmd() {
    local rc=$?
    printf '\e]133;D;%d\a' "$rc"
    if (( rc )); then
        printf '\a'
    fi
    printf '\e]133;A\a'
    local input_start=$'%{\e]133;B\a%}'
    if [[ $PS1 != *"$input_start" ]]; then
        PS1+=$input_start
    fi
    return 0
}
__mypixels_preexec() {
    printf '\e]133;C\a'
}
autoload -U add-zsh-hook
add-zsh-hook precmd __mypixels_precmd
add-zsh-hook preexec __mypixels_preexec
