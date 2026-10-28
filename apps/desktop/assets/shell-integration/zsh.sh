# CShell: current interactive zsh only; source or paste this block.
if [[ -o interactive && -n ${ZSH_VERSION-} && ${_CSHELL_INTEGRATED-} != 1 ]]; then
  _CSHELL_INTEGRATED=1
  autoload -Uz add-zsh-hook
  _cshell_precmd() {
    local result=$? cshell_directory=$PWD
    cshell_directory=${cshell_directory//\%/%25}; cshell_directory=${cshell_directory// /%20}; cshell_directory=${cshell_directory//$'\n'/%0A}; cshell_directory=${cshell_directory//$'\r'/%0D}; cshell_directory=${cshell_directory//$'\e'/%1B}; cshell_directory=${cshell_directory//$'\a'/%07}
    printf '\033]133;D;%s\007\033]7;file://localhost%s\007' "$result" "$cshell_directory"
    return 0
  }
  _cshell_preexec() { printf '\033]133;C\007'; }
  add-zsh-hook precmd _cshell_precmd
  add-zsh-hook preexec _cshell_preexec
  PS1=$'%{\e]133;A\a%}'"$PS1"$'%{\e]133;B\a%}'
fi
if [[ -o interactive && -n ${ZSH_VERSION-} && ${_CSHELL_INTEGRATED-} == 1 && ${_CSHELL_CONTINUATION-} != 1 ]]; then
  PS2=$'%{\e]133;P;k=s\a%}'"${PS2-}"$'%{\e]133;B\a%}'
  _CSHELL_CONTINUATION=1
fi
