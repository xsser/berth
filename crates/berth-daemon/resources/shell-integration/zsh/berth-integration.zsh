# berth shell integration for zsh (interactive shells, loaded by .zshenv).
#
# Prints escape sequences that berth reads from the terminal output:
#   OSC 133;A          a prompt is about to be drawn
#   OSC 133;B, 133;C   a command line was accepted and starts to run
#   OSC 133;D;<status> that command finished
#   OSC 7              the working directory (kitty-shell-cwd://host/path)
# Nothing else: prompt, aliases, options and key bindings are left alone.
# The hooks are installed at the first prompt, once the user's startup files
# have run, and go after the user's own precmd / preexec hooks.

# Loaded twice (e.g. re-sourced): keep the first copy.
(( ${+functions[__berth_precmd]} )) && return 0

# Set by preexec, cleared by the next precmd: a command ran in between.
typeset -gi __berth_running=0

__berth_report_cwd() {
  # A control character would end or garble the sequence: report nothing.
  [[ "$HOST$PWD" == *[[:cntrl:]]* ]] && return 0
  builtin print -rn -- $'\e]7;kitty-shell-cwd://'"$HOST$PWD"$'\a'
}

__berth_precmd() {
  # First: the status of the command that just finished.
  builtin local -i exit_status=$?
  builtin emulate -L zsh
  if (( __berth_running )); then
    __berth_running=0
    builtin print -rn -- $'\e]133;D;'"$exit_status"$'\a'
  fi
  __berth_report_cwd
  builtin print -rn -- $'\e]133;A\a'
}

__berth_preexec() {
  builtin emulate -L zsh
  __berth_running=1
  builtin print -rn -- $'\e]133;B\a\e]133;C\a'
}

__berth_first_prompt() {
  builtin emulate -L zsh
  precmd_functions=(${precmd_functions:#__berth_first_prompt} __berth_precmd)
  preexec_functions=(${preexec_functions:#__berth_preexec} __berth_preexec)
  builtin unfunction __berth_first_prompt
  __berth_precmd
}

precmd_functions+=(__berth_first_prompt)
