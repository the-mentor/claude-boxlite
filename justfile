set dotenv-load

base_tag   := "claude-boxlite-base"
custom_tag := "claude-boxlite-custom"
registry   := "localhost:5000"
box_name   := "claude-box"
disk_size  := "10"
compose    := "docker compose -f local-development/registry/docker-compose.yml"

default:
    @just --list

# Start the local image registry (docker compose)
registry-up:
    {{compose}} up -d

# Stop the local image registry
registry-down:
    {{compose}} down

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
    home="${BOXLITE_HOME:-$HOME/.boxlite}"
    db="$home/db/boxlite.db"
    img="$home/images"
    command -v sqlite3 >/dev/null 2>&1 || { echo "clean-cache: sqlite3 not found; skipping" >&2; exit 0; }
    [ -f "$db" ] || { echo "clean-cache: no boxlite db at $db; skipping" >&2; exit 0; }
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

# Build images, then boot the box and launch Claude Code.
# Pass -f/--force to replace an existing box of the same name.
# Usage: just up-dev [box-name] [-f|--force]
up-dev name=box_name force="": build (up name force)

# Boot the box and launch Claude Code (assumes images are already built).
# Pass -f/--force to replace an existing box of the same name (boxlite run has no --force).
# Usage: just up [box-name] [-f|--force]
up name=box_name force="":
    #!/usr/bin/env sh
    case "{{force}}" in
      -f|--force) boxlite rm -f {{name}} 2>/dev/null || true ;;
      "") ;;
      *) echo "unknown option: {{force}} (use -f or --force)" >&2; exit 2 ;;
    esac
    exec boxlite run -it --name {{name}} --disk-size {{disk_size}} --config registries.json -w /workspace -e CLAUDE_CODE_OAUTH_TOKEN {{custom_tag}} claude

# Open a session in the running box
# Usage: just shell [box-name]
shell name=box_name:
    boxlite exec -it -w /workspace {{name}} -- claude

# Stop and remove the box
# Usage: just down [box-name]
down name=box_name:
    -boxlite stop {{name}}
    -boxlite rm {{name}}
