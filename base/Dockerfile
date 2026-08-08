FROM node:26-trixie-slim
RUN apt update \
 && apt upgrade -y \
 && apt install -y --no-install-recommends git curl wget ca-certificates python3 python3-pip just \
 && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
 && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list \
 && apt update \
 && apt install -y --no-install-recommends gh \
 && rm -rf /var/lib/apt/lists/*

RUN npm install -g @anthropic-ai/claude-code

ARG UV_VERSION=0.12.3
RUN pip install --no-cache-dir --break-system-packages "uv==${UV_VERSION}"

WORKDIR /workspace
