FROM node:20-bookworm-slim
RUN apt update \
 && apt upgrade -y \
 && apt install -y --no-install-recommends git ca-certificates gh python3 python3-pip \
 && rm -rf /var/lib/apt/lists/*

RUN npm install -g @anthropic-ai/claude-code

WORKDIR /workspace
