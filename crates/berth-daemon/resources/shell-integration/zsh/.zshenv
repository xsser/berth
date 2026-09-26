# berth shell integration for zsh.
#
# berthd starts interactive zsh sessions with ZDOTDIR pointing at this
# directory, so zsh reads this file as its .zshenv. It puts the user's
# ZDOTDIR back first: zsh then reads the user's .zprofile, .zshrc and
# .zlogin itself, from where it would have without berth. This file sources
# the user's .zshenv and, in interactive shells, berth-integration.zsh.
#
# BERTH_ORIG_ZDOTDIR is the user's ZDOTDIR; empty means it was not set.
# Command words are quoted so that no alias can replace them.

if [[ -n "${BERTH_ORIG_ZDOTDIR-}" ]]; then
  'builtin' 'export' ZDOTDIR="$BERTH_ORIG_ZDOTDIR"
else
  'builtin' 'unset' ZDOTDIR
fi
'builtin' 'unset' BERTH_ORIG_ZDOTDIR

{
  # At top level, as zsh would read it: the user's typesets stay global.
  if [[ -r "${ZDOTDIR-$HOME}/.zshenv" ]]; then
    if [[ -o function_argzero ]]; then
      # zsh reads startup files with $0 unchanged; `source` would set it to
      # the file's name. (A .zshenv that itself turns FUNCTION_ARGZERO off
      # gets it back on: that cannot be told apart.)
      'builtin' 'unsetopt' function_argzero
      'builtin' 'source' -- "${ZDOTDIR-$HOME}/.zshenv"
      'builtin' 'setopt' function_argzero
    else
      'builtin' 'source' -- "${ZDOTDIR-$HOME}/.zshenv"
    fi
  fi
} always {
  if [[ -o interactive ]]; then
    # Parse berth's functions with zsh defaults and without the aliases the
    # user's .zshenv may have defined.
    () {
      'builtin' 'emulate' -L zsh -o no_aliases
      'builtin' 'source' -- "$1"
    } "${${(%):-%x}:A:h}/berth-integration.zsh"
  fi
}
