# Enable bash-completion for interactive non-login shells (e.g. boxlite exec).
if [ -f /usr/share/bash-completion/bash_completion ]; then
    . /usr/share/bash-completion/bash_completion
fi

# Custom Aliases
alias lg=lazygit
