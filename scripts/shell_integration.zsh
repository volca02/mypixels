# mypixels shell integration (zsh): command marks + a bell on failure.
#
#   source /path/to/this/file   (e.g. from ~/.zshrc)
#
# OSC 133: precmd reports D (with the exit status, captured first thing)
# and A (new prompt); preexec reports C (command execution begins). See
# shell_integration.bash for the bell's meaning - escalation to attention
# + red tab pulse when the bell is unseen. Sourcing twice is harmless.
__mypixels_precmd() {
    local rc=$?
    printf '\e]133;D;%d\a' "$rc"
    if (( rc )); then
        printf '\a'
    fi
    printf '\e]133;A\a'
}
__mypixels_preexec() {
    printf '\e]133;B\a\e]133;C\a'
}
autoload -U add-zsh-hook
if [[ -z ${functions[__mypixels_precmd]} ]] || [[ ${precmd_functions[(I)__mypixels_precmd]} -eq 0 ]]; then
    add-zsh-hook precmd __mypixels_precmd
    add-zsh-hook preexec __mypixels_preexec
fi
