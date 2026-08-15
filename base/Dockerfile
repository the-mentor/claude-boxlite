FROM node:26-trixie-slim
RUN apt update \
    && apt upgrade -y \
    && apt install -y --no-install-recommends git curl wget ca-certificates python3 python3-pip just unzip jq bash-completion \
    && rm -rf /var/lib/apt/lists/*

# /etc/profile.d hooks (incl. bash-completion's) only run for login shells;
# boxlite's interactive session is a non-login shell, so source our own
# customizations file from /etc/bash.bashrc too. Add future bashrc tweaks to
# bashrc.d/boxlite.sh rather than growing this RUN step.
COPY bashrc.d/boxlite.sh /etc/bashrc.d/boxlite.sh
RUN echo '[ -f /etc/bashrc.d/boxlite.sh ] && . /etc/bashrc.d/boxlite.sh' >> /etc/bash.bashrc

RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list \
    && apt update \
    && apt install -y --no-install-recommends gh \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g @anthropic-ai/claude-code

RUN curl -s https://ohmyposh.dev/install.sh | bash -s -- -d /usr/local/bin

# Wrap the claude binary to pre-approve ANTHROPIC_API_KEY in ~/.claude.json at
# startup, silencing the "Detected a custom API key" interactive prompt in
# ephemeral boxes where the config file starts fresh on every boot.
COPY --chmod=0755 claude-wrapper.sh /usr/local/bin/claude-wrapper
RUN mv /usr/local/bin/claude /usr/local/bin/claude-real \
    && ln -sf /usr/local/bin/claude-wrapper /usr/local/bin/claude \
    && ln -sf /usr/local/bin/claude-wrapper /usr/local/bin/cc

ARG UV_VERSION=0.12.3
RUN pip install --no-cache-dir --break-system-packages "uv==${UV_VERSION}"

WORKDIR /workspace
