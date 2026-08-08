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

build: build-image

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
