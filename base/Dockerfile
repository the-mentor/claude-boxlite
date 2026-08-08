FROM node:20-bookworm-slim
RUN apt update \
 && apt upgrade -y \
 && apt install -y --no-install-recommends git ca-certificates gh python3 python3-pip \
 && rm -rf /var/lib/apt/lists/*

RUN npm install -g @anthropic-ai/claude-code

ARG UV_VERSION=0.12.3
RUN pip install --no-cache-dir --break-system-packages "uv==${UV_VERSION}"

WORKDIR /workspace
