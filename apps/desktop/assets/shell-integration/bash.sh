# CShell: current interactive shell only; Bash 4.4+; source or paste this block.
if [[ $- == *i* && -n ${BASH_VERSION-}  ]] && (( BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4) )) && [[ ${_CSHELL_INTEGRATED-} != 1 ]]; then
  _CSHELL_INTEGRATED=1
  _cshell_prompt() {
    local result=$? cshell_directory=$PWD
    cshell_directory=${cshell_directory//%/%25}; cshell_directory=${cshell_directory// /%20}; cshell_directory=${cshell_directory//$'\n'/%0A}; cshell_directory=${cshell_directory//$'\r'/%0D}; cshell_directory=${cshell_directory//$'\e'/%1B}; cshell_directory=${cshell_directory//$'\a'/%07}
    printf '\033]133;D;%s\007\033]7;file://localhost%s\007' "$result" "$cshell_directory"
    return "$result"
  }
  if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then
    PROMPT_COMMAND=(_cshell_prompt "${PROMPT_COMMAND[@]}")
  else
    PROMPT_COMMAND="_cshell_prompt${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
  fi
  PS0=$'\e]133;C\a'"${PS0-}"
  PS1=$'\[\e]133;A\a\]'"$PS1"$'\[\e]133;B\a\]'
fi
if [[ $- == *i* && -n ${BASH_VERSION-} && ${_CSHELL_INTEGRATED-} == 1 && ${_CSHELL_CONTINUATION-} != 1 ]]; then
  PS2=$'\[\e]133;P;k=s\a\]'"${PS2-}"$'\[\e]133;B\a\]'
  _CSHELL_CONTINUATION=1
fi
