set dotenv-load

base_tag   := "claude-boxlite-base"
custom_tag := "claude-boxlite-custom"
registry   := "localhost:5551"
box_name   := "claude-box"
disk_size  := "10"
# Env vars passed into the box when set (in .env via dotenv-load, or the host
# env). Unset ones are skipped — there's no way to fall back to a default here,
# which is why TERM itself is NOT in this list (see the "TERM=..." flag on the
# `boxlite run`/`exec` invocations below): TERM needs a guaranteed value
# (defaults to xterm-256color) so the box always renders in color even when
# the host's TERM is unset. Add a var here to make it available in the box.
#
# The TERM_PROGRAM/... group below identifies the actual terminal emulator
# (TERM alone is often just "xterm-256color" for all of them). Claude Code
# uses TERM_PROGRAM to decide whether to enable the Kitty keyboard protocol,
# which is what lets a terminal tell Shift+Enter apart from plain Enter —
# without it, Shift+Enter silently behaves like Enter inside the box even in
# terminals (iTerm2, WezTerm, Warp) where it works fine outside the box.
passthrough_vars := "CLAUDE_CODE_OAUTH_TOKEN ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_BASE_URL ANTHROPIC_MODEL GH_TOKEN GITHUB_TOKEN GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL TERM_PROGRAM TERM_PROGRAM_VERSION COLORTERM KITTY_WINDOW_ID WEZTERM_EXECUTABLE ITERM_SESSION_ID WT_SESSION VTE_VERSION"
compose    := "docker compose -f local-development/registry/docker-compose.yml"
gateway    := "docker compose -f agentgateway/docker-compose.yml --env-file .env"

default:
    @just --list

# Install the boxlite CLI itself (a prerequisite for this repo) by downloading
# the release tarball directly from GitHub (no curl|sh pipe) and verifying its
# sha256 checksum before installing. Installs the latest release by default;
# pass a version (e.g. v0.9.7) to pin. Installs into ~/bin by default; pass a
# directory to install elsewhere.
# Usage: just install-boxlite [version] [dir]
install-boxlite version="" dir=(env_var('HOME') + "/bin"):
    #!/usr/bin/env sh
    set -eu
    repo="boxlite-ai/boxlite"
    install_dir="{{dir}}"

    case "$(uname -s)-$(uname -m)" in
      Darwin-arm64) target="aarch64-apple-darwin" ;;
      Darwin-x86_64) echo "install-boxlite: macOS Intel is not supported; BoxLite requires Apple Silicon" >&2; exit 1 ;;
      Linux-x86_64) target="x86_64-unknown-linux-gnu" ;;
      Linux-aarch64|Linux-arm64) target="aarch64-unknown-linux-gnu" ;;
      *) echo "install-boxlite: unsupported platform $(uname -s)-$(uname -m)" >&2; exit 1 ;;
    esac

    fetch() {
      # $1 = url, $2 = output path
      if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
      elif command -v wget >/dev/null 2>&1; then
        wget -qO "$2" "$1"
      else
        echo "install-boxlite: need curl or wget" >&2; exit 1
      fi
    }

    version="{{version}}"
    if [ -z "$version" ]; then
      echo "Resolving latest boxlite release..." >&2
      tmp_release="$(mktemp)"
      fetch "https://api.github.com/repos/${repo}/releases/latest" "$tmp_release"
      version="$(grep -m1 '"tag_name"' "$tmp_release" | sed -E 's/.*"tag_name":[[:space:]]*"([^"]+)".*/\1/')"
      rm -f "$tmp_release"
      [ -n "$version" ] || { echo "install-boxlite: could not resolve latest version" >&2; exit 1; }
    fi

    archive="boxlite-cli-${version}-${target}.tar.gz"
    base_url="https://github.com/${repo}/releases/download/${version}"

    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    echo "Downloading ${archive} (${version})..." >&2
    fetch "${base_url}/${archive}" "${tmpdir}/${archive}"
    fetch "${base_url}/${archive}.sha256" "${tmpdir}/${archive}.sha256"

    expected="$(awk '{print $1}' "${tmpdir}/${archive}.sha256")"
    if command -v sha256sum >/dev/null 2>&1; then
      actual="$(sha256sum "${tmpdir}/${archive}" | awk '{print $1}')"
    else
      actual="$(shasum -a 256 "${tmpdir}/${archive}" | awk '{print $1}')"
    fi
    [ "$actual" = "$expected" ] || { echo "install-boxlite: checksum mismatch (expected $expected, got $actual)" >&2; exit 1; }

    mkdir -p "$install_dir"
    tar --no-same-owner -xzf "${tmpdir}/${archive}" -C "$tmpdir" boxlite
    install -m 0755 "${tmpdir}/boxlite" "${install_dir}/boxlite"
    echo "Installed ${install_dir}/boxlite (${version})" >&2
    case ":$PATH:" in
      *":${install_dir}:"*) : ;;
      *) echo "Note: ${install_dir} is not on your PATH. Add this to your shell rc file:" >&2
         echo "  export PATH=\"${install_dir}:\$PATH\"" >&2 ;;
    esac

# Symlink the cb wrapper (bin/cb) onto PATH so `cb up-dev` etc. work from
# any directory. Installs into ~/bin by default; pass a directory to
# install elsewhere.
# Usage: just install [dir]
install dir=(env_var('HOME') + "/bin"):
    #!/usr/bin/env sh
    set -eu
    mkdir -p "{{dir}}"
    ln -sf "{{justfile_directory()}}/bin/cb" "{{dir}}/cb"
    echo "Installed {{dir}}/cb -> {{justfile_directory()}}/bin/cb"
    case ":$PATH:" in
      *":{{dir}}:"*) : ;;
      *) echo "Note: {{dir}} is not on your PATH. Add this to your shell rc file:" >&2
         echo "  export PATH=\"{{dir}}:\$PATH\"" >&2 ;;
    esac

# Remove the symlink installed by `just install`.
# Usage: just uninstall [dir]
uninstall dir=(env_var('HOME') + "/bin"):
    rm -f "{{dir}}/cb"

# Start the local image registry (docker compose)
registry-up:
    {{compose}} up -d

# Stop the local image registry
registry-down:
    {{compose}} down

# Start the host-side agentgateway (docker compose). Long-lived: boxes come and
# go, this stays up. Not started by `just up`/`up-dev`.
gateway-up:
    {{gateway}} up -d

# Stop the host-side agentgateway
gateway-down:
    {{gateway}} down

# Follow the agentgateway logs
gateway-logs:
    {{gateway}} logs -f

# Log in to an authenticated image registry (e.g. ECR) and store credentials in
# registries.local.json (gitignored). Mirrors `docker login`'s interface.
# Usage: aws ecr get-login-password --region <region> | just registry-login --registry <host> --username AWS --password-stdin
registry-login *args:
    ./scripts/registry-login.py {{args}}

build-base:
    docker build -t {{base_tag}} base/

build-image: build-base registry-up
    docker build -t {{custom_tag}} custom/
    docker tag {{custom_tag}} {{registry}}/library/{{custom_tag}}
    docker push {{registry}}/library/{{custom_tag}}
    just clean-cache

build: build-image

# Refresh the custom image and sweep orphaned image blobs from boxlite's cache.
# BoxLite caches image tags immutably and has no `rmi`, so a rebuilt :latest is
# ignored until its cached tag->digest row is dropped; then the next
# `boxlite run` re-pulls from the registry. We also delete blob files
# (manifests/configs/layers/extracted) no longer referenced by any image in
# boxlite's index. Disk-images are left alone on purpose: boxlite does not
# record which image a disk-image belongs to, so an orphaned one can't be told
# apart from a live one without risking a costly (or breaking) re-pull.
clean-cache:
    #!/usr/bin/env sh
    set -eu
    command -v sqlite3 >/dev/null 2>&1 || { echo "clean-cache: sqlite3 not found; skipping" >&2; exit 0; }
    root="${BOXLITE_HOME:-$HOME/.boxlite}/boxes"
    [ -d "$root" ] || { echo "clean-cache: no box homes under $root; skipping" >&2; exit 0; }
    for dir in "$root"/*/; do
      [ -d "$dir" ] || continue
      home="${dir%/}"
      db="$home/db/boxlite.db"
      img="$home/images"
      [ -f "$db" ] || continue
      # Drop the custom tag so the next `boxlite run` re-pulls the pushed image.
      sqlite3 "$db" "DELETE FROM image_index WHERE reference='{{registry}}/library/{{custom_tag}}:latest';"
      # Blobs still referenced by any remaining image (filename form: sha256-...).
      keep="$(mktemp)"
      { sqlite3 "$db" "SELECT manifest_digest FROM image_index;"
        sqlite3 "$db" "SELECT config_digest FROM image_index;"
        sqlite3 "$db" "SELECT value FROM image_index, json_each(layers);"
      } | tr ':' '-' | sort -u > "$keep"
      for f in "$img"/manifests/* "$img"/configs/* "$img"/layers/* "$img"/extracted/*; do
        [ -e "$f" ] || continue
        key="$(basename "$f" | sed 's/\.json$//; s/\.tar\.gz$//')"
        grep -qx "$key" "$keep" || rm -rf "$f"
      done
      rm -f "$keep"
    done

# Build images, then boot the box and launch Claude Code.
# Pass -f/--force to replace an existing box of the same name.
# Usage: just up-dev [box-name] [-f|--force] [-c|--cwd] [-v host:box ...] [-e KEY=VALUE ...]
up-dev *args=box_name: build (up args)

# Boot the box and launch Claude Code (assumes images are already built).
# Pass -f/--force to replace an existing box of the same name (boxlite run has no --force).
# Pass -c/--cwd to mount the host current directory onto /workspace.
# Pass -v/--volume host:box (repeatable) to mount a host folder into the box.
# Pass -e/--env KEY=VALUE (repeatable) to inject an extra environment variable into the box.
# Pass -- <cmd> to override the executable launched in the box (default: claude).
# Usage: just up [box-name] [-f|--force] [-c|--cwd] [-v host:box ...] [-e KEY=VALUE ...] [-- cmd...]
#
# Each box name gets its own BOXLITE_HOME (${BOXLITE_HOME:-$HOME/.boxlite}/boxes/<name>).
# boxlite takes an exclusive lock on the whole BOXLITE_HOME directory for as long as a
# `boxlite run`/`exec` process is attached to it (not just on the one box), so two boxes
# sharing a home can't run at once. Splitting the home per box name is what lets
# `just up box-a` and `just up box-b` run concurrently. (A shared `boxlite serve` daemon
# would avoid the per-process lock entirely, but its REST API doesn't yet forward
# `-v`/`-c` bind mounts to the box - boxlite-ai/boxlite#942 - so it can't replace this
# yet.) Each box name pays for its own image cache under its home dir; `just clean-cache`
# sweeps all of them.
up *args=box_name:
    #!/usr/bin/env sh
    set -eu
    set -- {{args}}
    name={{box_name}}; force=""; vols=""; exec_cmd="claude"; extra_envflags=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --) shift; exec_cmd="$*"; break ;;
        -f|--force) force=1 ;;
        -c|--cwd) vols="$vols -v {{invocation_directory()}}:/workspace" ;;
        -v|--volume) shift; vols="$vols -v $1" ;;
        -e|--env) shift; extra_envflags="$extra_envflags -e $1" ;;
        -*) echo "unknown option: $1" >&2; exit 2 ;;
        *) name="$1" ;;
      esac
      shift
    done
    home="${BOXLITE_HOME:-$HOME/.boxlite}/boxes/$name"
    [ -n "$force" ] && boxlite --home "$home" rm -f "$name" 2>/dev/null || true
    [ -f registries.local.json ] || cp registries.json registries.local.json
    envflags=""
    for v in {{passthrough_vars}}; do
      eval "val=\${$v:-}"
      [ -n "$val" ] && envflags="$envflags -e $v"
    done
    envflags="$envflags$extra_envflags"
    boxlite --home "$home" run -it --name "$name" --disk-size {{disk_size}} $vols --config registries.local.json -w /workspace $envflags -e "TERM=${TERM:-xterm-256color}" -e "BOX_NAME=$name" {{custom_tag}} -- $exec_cmd

# List running boxes across every box-name home under ${BOXLITE_HOME:-$HOME/.boxlite}/boxes.
# Usage: just list [args...]
list *args:
    #!/usr/bin/env sh
    set -eu
    root="${BOXLITE_HOME:-$HOME/.boxlite}/boxes"
    [ -d "$root" ] || exit 0
    for dir in "$root"/*/; do
      [ -d "$dir" ] || continue
      echo "== $(basename "$dir") =="
      boxlite --home "$dir" list {{args}}
    done

# Open a session in the running box.
# Pass -- <cmd> to override the executable launched in the box (default: claude).
# Note: `boxlite exec` also opens its own local runtime and takes the same per-home lock
# as `boxlite run`, so this only succeeds once that lock is free - i.e. once the `just up`
# session for this box has exited (this was already true before per-box homes; it's a
# limitation of the boxlite CLI's process model, not something this recipe adds).
# `shell` is an alias for this recipe.
# Usage: just exec [box-name] [-- cmd...]
# Usage: just shell [box-name] [-- cmd...]
alias shell := exec
exec *args=box_name:
    #!/usr/bin/env sh
    set -eu
    set -- {{args}}
    name={{box_name}}; exec_cmd="claude"
    while [ $# -gt 0 ]; do
      case "$1" in
        --) shift; exec_cmd="$*"; break ;;
        -*) echo "unknown option: $1" >&2; exit 2 ;;
        *) name="$1" ;;
      esac
      shift
    done
    home="${BOXLITE_HOME:-$HOME/.boxlite}/boxes/$name"
    envflags=""
    for v in {{passthrough_vars}}; do
      eval "val=\${$v:-}"
      [ -n "$val" ] && envflags="$envflags -e $v"
    done
    boxlite --home "$home" exec -it --config registries.local.json -w /workspace $envflags -e "TERM=${TERM:-xterm-256color}" -e "BOX_NAME=$name" "$name" -- $exec_cmd

# Stop and remove the box
# Usage: just down [box-name]
down name=box_name:
    #!/usr/bin/env sh
    set -eu
    home="${BOXLITE_HOME:-$HOME/.boxlite}/boxes/{{name}}"
    boxlite --home "$home" stop {{name}} || true
    boxlite --home "$home" rm {{name}} || true
