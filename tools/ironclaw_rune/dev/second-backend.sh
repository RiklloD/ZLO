#!/bin/sh
# Run an isolated second real ZLO backend for testing multi-computer support.
#
# Usage:
#   sh tools/ironclaw_rune/dev/second-backend.sh start [--linux] [--port N]
#   sh tools/ironclaw_rune/dev/second-backend.sh status
#   sh tools/ironclaw_rune/dev/second-backend.sh smoke
#   sh tools/ironclaw_rune/dev/second-backend.sh pair
#   sh tools/ironclaw_rune/dev/second-backend.sh stop
#   sh tools/ironclaw_rune/dev/second-backend.sh clean
#
# Environment:
#   ZLO_SECOND_HOME   Data folder (default: ${TMPDIR:-/tmp}/zlo-second-backend)
#   DOCKER_HOST       Docker socket for --linux (auto-detects colima on macOS)
set -eu

DEFAULT_PORT=3310
CONTAINER_NAME="zlo-second-backend"
DATA_VOLUME="zlo-second-backend-data"
TARGET_VOLUME="zlo-second-backend-target"
REGISTRY_VOLUME="zlo-second-backend-registry"
GIT_VOLUME="zlo-second-backend-git"
DESCRIPTOR_PATH="/.well-known/ironclaw/environment"
INBOX_PATH="/api/webchat/v2/inbox"

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../../.." && pwd)
ZLO_BIN="$REPO_ROOT/target/debug/zlo"

TMP_BASE="${TMPDIR:-/tmp}"
TMP_BASE="${TMP_BASE%/}"
DATA_DIR="${ZLO_SECOND_HOME:-$TMP_BASE/zlo-second-backend}"

PID_FILE="$DATA_DIR/second-backend.pid"
MODE_FILE="$DATA_DIR/second-backend.mode"
PORT_FILE="$DATA_DIR/second-backend.port"
LOG_FILE="$DATA_DIR/second-backend.log"

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: second-backend.sh <command> [options]

Commands:
  start [--linux] [--port N]  Start an isolated second ZLO backend (default port: 3310)
  status                      Show running state, URL, environment id and label
  smoke                       Verify descriptor (200), unauth inbox (401), auth inbox (200)
  pair                        Mint a one-time pairing code and print the link/code
  stop                        Stop the second backend
  clean                       Stop and delete its data folder / Docker data volume

Environment:
  ZLO_SECOND_HOME             Isolated data folder (default: ${TMPDIR:-/tmp}/zlo-second-backend)
EOF
}

validate_data_dir() {
  case "$DATA_DIR" in
    ""|/|/*../*|*/..|../*|..)
      die "ZLO_SECOND_HOME must be a clean absolute path (got: '$DATA_DIR')"
      ;;
    /*) ;;
    *)
      die "ZLO_SECOND_HOME must be an absolute path (got: '$DATA_DIR')"
      ;;
  esac

  real_home="${HOME:-}"
  if [ -n "$real_home" ]; then
    real_home="${real_home%/}"
    case "$DATA_DIR" in
      "$real_home/.zlo"|"$real_home/.zlo/"*|\
      "$real_home/.ironclaw"|"$real_home/.ironclaw/"*|\
      "$real_home/Library/Application Support/ZLO"|"$real_home/Library/Application Support/ZLO/"*)
        die "refusing to use developer ZLO/IronClaw data path: $DATA_DIR"
        ;;
    esac
  fi
}

validate_port() {
  port_val="$1"
  case "$port_val" in
    ""|*[!0-9]*)
      die "invalid port '$port_val' (must be an integer 1..65535)"
      ;;
  esac
  if [ "$port_val" -lt 1 ] || [ "$port_val" -gt 65535 ]; then
    die "invalid port '$port_val' (must be 1..65535)"
  fi
  if [ "$port_val" -eq 3000 ]; then
    die "refusing default port 3000 to avoid colliding with the main ZLO backend; pick another port with --port"
  fi
}

port_in_use() {
  check_port="$1"
  if command -v lsof >/dev/null 2>&1; then
    if lsof -nP -iTCP:"$check_port" -sTCP:LISTEN >/dev/null 2>&1; then
      return 0
    fi
  fi
  if command -v python3 >/dev/null 2>&1; then
    if ! python3 - "$check_port" <<'PY' 2>/dev/null
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
try:
    s.bind(("127.0.0.1", int(sys.argv[1])))
except OSError:
    sys.exit(1)
finally:
    s.close()
PY
    then
      return 0
    fi
  elif curl --silent --connect-timeout 1 "http://127.0.0.1:$check_port/" >/dev/null 2>&1; then
    return 0
  fi
  return 1
}

ensure_docker() {
  if [ -z "${DOCKER_HOST:-}" ] && [ -S "${HOME:-}/.colima/default/docker.sock" ]; then
    export DOCKER_HOST="unix://${HOME}/.colima/default/docker.sock"
  fi
  if ! command -v docker >/dev/null 2>&1; then
    die "docker CLI not found in PATH"
  fi
  if ! docker info >/dev/null 2>&1; then
    die "Docker is not answering (on macOS run 'colima start' and export DOCKER_HOST=unix://\$HOME/.colima/default/docker.sock)"
  fi
}

docker_available() {
  if [ -z "${DOCKER_HOST:-}" ] && [ -S "${HOME:-}/.colima/default/docker.sock" ]; then
    export DOCKER_HOST="unix://${HOME}/.colima/default/docker.sock"
  fi
  command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1
}

generate_master_key_hex() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 32
  elif [ -r /dev/urandom ]; then
    od -An -tx1 -N32 /dev/urandom | tr -d ' \n'
    printf '\n'
  else
    die "cannot generate random master key (need openssl or /dev/urandom)"
  fi
}

json_field() {
  field="$1"
  if command -v jq >/dev/null 2>&1; then
    jq -r --arg k "$field" '.[$k] // empty'
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c 'import json, sys; d = json.load(sys.stdin); v = d.get(sys.argv[1]); print("" if v is None else v)' "$field"
  else
    die "need jq or python3 to parse JSON"
  fi
}

current_mode() {
  if [ -f "$MODE_FILE" ]; then
    cat "$MODE_FILE"
  else
    printf 'native\n'
  fi
}

current_port() {
  if [ -f "$PORT_FILE" ]; then
    cat "$PORT_FILE"
  else
    printf '%s\n' "$DEFAULT_PORT"
  fi
}

is_native_running() {
  if [ ! -f "$PID_FILE" ]; then
    return 1
  fi
  pid=$(cat "$PID_FILE" 2>/dev/null || true)
  case "$pid" in
    ""|*[!0-9]*) return 1 ;;
  esac
  kill -0 "$pid" 2>/dev/null
}

is_linux_running() {
  if ! docker_available; then
    return 1
  fi
  state=$(docker inspect -f '{{.State.Running}}' "$CONTAINER_NAME" 2>/dev/null || true)
  [ "$state" = "true" ]
}

is_running() {
  mode=$(current_mode)
  if [ "$mode" = "linux" ]; then
    is_linux_running
  else
    is_native_running
  fi
}

run_native_zlo() {
  env \
    -u IRONCLAW_REBORN_HOME \
    -u IRONCLAW_BASE_DIR \
    -u ZLO_WEBUI_TOKEN \
    -u IRONCLAW_REBORN_WEBUI_TOKEN \
    -u ZLO_PROFILE \
    -u IRONCLAW_REBORN_PROFILE \
    -u SECRETS_MASTER_KEY \
    ZLO_HOME="$DATA_DIR" \
    HOME="$DATA_DIR/home" \
    XDG_CONFIG_HOME="$DATA_DIR/home/.config" \
    XDG_DATA_HOME="$DATA_DIR/home/.local/share" \
    "$ZLO_BIN" "$@"
}

write_config_toml() {
  cfg_path="$1"
  cfg_port="$2"
  cat >"$cfg_path" <<EOF
api_version = "ironclaw.runtime/v1"

[boot]
profile = "local-dev"

[webui]
listen_host = "127.0.0.1"
listen_port = $cfg_port
EOF
}

wait_for_descriptor() {
  wait_port="$1"
  wait_mode="$2"
  tries=0
  max_tries=60
  while [ "$tries" -lt "$max_tries" ]; do
    code=$(curl --silent --output /dev/null --write-out '%{http_code}' \
      --connect-timeout 1 --max-time 2 \
      "http://127.0.0.1:$wait_port$DESCRIPTOR_PATH" || true)
    if [ "$code" = "200" ]; then
      return 0
    fi
    if [ "$wait_mode" = "native" ]; then
      if ! is_native_running; then
        printf 'error: second backend exited before becoming ready. Last log lines:\n' >&2
        tail -n 40 "$LOG_FILE" >&2 || true
        return 1
      fi
    else
      if ! is_linux_running; then
        printf 'error: Linux container exited before becoming ready. Last log lines:\n' >&2
        docker logs --tail 40 "$CONTAINER_NAME" >&2 || true
        return 1
      fi
    fi
    tries=$((tries + 1))
    sleep 1
  done
  printf 'error: timed out waiting for http://127.0.0.1:%s%s\n' "$wait_port" "$DESCRIPTOR_PATH" >&2
  if [ "$wait_mode" = "native" ]; then
    tail -n 40 "$LOG_FILE" >&2 || true
  else
    docker logs --tail 40 "$CONTAINER_NAME" >&2 || true
  fi
  return 1
}

ensure_linux_image() {
  toolchain=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$REPO_ROOT/rust-toolchain.toml")
  if [ -z "$toolchain" ]; then
    die "could not read Rust toolchain channel from $REPO_ROOT/rust-toolchain.toml"
  fi
  LINUX_IMAGE="zlo-second-backend:$toolchain"
  if ! docker image inspect "$LINUX_IMAGE" >/dev/null 2>&1; then
    printf 'Building Docker image %s...\n' "$LINUX_IMAGE"
    docker build -t "$LINUX_IMAGE" - <<DOCKERFILE
FROM rust:1-bookworm
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
    pkg-config libssl-dev ca-certificates curl socat iproute2 \
    && rm -rf /var/lib/apt/lists/*
RUN rustup toolchain install $toolchain --profile minimal
DOCKERFILE
  fi
}

cmd_start() {
  mode="native"
  port="$DEFAULT_PORT"
  while [ $# -gt 0 ]; do
    case "$1" in
      --linux)
        mode="linux"
        shift
        ;;
      --port)
        [ $# -ge 2 ] || die "--port requires a value"
        port="$2"
        shift 2
        ;;
      --port=*)
        port="${1#--port=}"
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        die "unknown start option: $1"
        ;;
    esac
  done

  validate_port "$port"

  if is_native_running; then
    die "second backend is already running natively (pid $(cat "$PID_FILE"), port $(current_port)); stop it first"
  fi
  if is_linux_running; then
    die "second backend is already running in Docker ($CONTAINER_NAME, port $(current_port)); stop it first"
  fi

  if port_in_use "$port"; then
    die "port $port is already in use on 127.0.0.1"
  fi

  mkdir -p "$DATA_DIR/home" "$DATA_DIR/local-dev" "$DATA_DIR/workspace"
  chmod 700 "$DATA_DIR" "$DATA_DIR/home" "$DATA_DIR/local-dev" "$DATA_DIR/workspace"

  if [ "$mode" = "native" ]; then
    if [ ! -x "$ZLO_BIN" ]; then
      printf 'Building debug zlo binary...\n'
      (cd "$REPO_ROOT" && SKIP_FRONTEND_BUILD=1 cargo build -p ironclaw --bin zlo)
    fi

    master_key_file="$DATA_DIR/local-dev/.reborn-local-dev-secrets-master-key"
    if [ ! -f "$master_key_file" ]; then
      umask 077
      generate_master_key_hex >"$master_key_file"
      chmod 600 "$master_key_file"
    fi

    write_config_toml "$DATA_DIR/config.toml" "$port"
    printf 'native\n' >"$MODE_FILE"
    printf '%s\n' "$port" >"$PORT_FILE"

    (
      cd "$DATA_DIR/workspace"
      nohup env \
        -u IRONCLAW_REBORN_HOME \
        -u IRONCLAW_BASE_DIR \
        -u ZLO_WEBUI_TOKEN \
        -u IRONCLAW_REBORN_WEBUI_TOKEN \
        -u ZLO_PROFILE \
        -u IRONCLAW_REBORN_PROFILE \
        -u SECRETS_MASTER_KEY \
        ZLO_HOME="$DATA_DIR" \
        HOME="$DATA_DIR/home" \
        XDG_CONFIG_HOME="$DATA_DIR/home/.config" \
        XDG_DATA_HOME="$DATA_DIR/home/.local/share" \
        "$ZLO_BIN" serve --host 127.0.0.1 --port "$port" --create-token-if-missing \
        >>"$LOG_FILE" 2>&1 </dev/null &
      printf '%s\n' "$!" >"$PID_FILE"
    )

    wait_for_descriptor "$port" "native"
  else
    ensure_docker
    ensure_linux_image

    # Remove any stopped container with the same name.
    docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true

    # Build the Linux debug binary into the named target volume if missing.
    if ! docker run --rm \
      -v "$TARGET_VOLUME":/build \
      "$LINUX_IMAGE" test -x /build/debug/zlo; then
      printf 'Building Linux debug zlo binary in Docker (cached in %s)...\n' "$TARGET_VOLUME"
      docker run --rm \
        -v "$REPO_ROOT":/src:ro \
        -v "$TARGET_VOLUME":/build \
        -v "$REGISTRY_VOLUME":/usr/local/cargo/registry \
        -v "$GIT_VOLUME":/usr/local/cargo/git \
        -e CARGO_TARGET_DIR=/build \
        -e SKIP_FRONTEND_BUILD=1 \
        -w /src \
        "$LINUX_IMAGE" \
        cargo build -p ironclaw --bin zlo
    fi

    printf 'linux\n' >"$MODE_FILE"
    printf '%s\n' "$port" >"$PORT_FILE"

    # Inside the container, zlo serve stays on 127.0.0.1:$port (preserving the
    # loopback-only Standalone policy and --create-token-if-missing), while
    # socat forwards the container's eth0 IP on the same port to 127.0.0.1:$port
    # so Docker's -p 127.0.0.1:$port:$port reaches it.
    docker run -d \
      --name "$CONTAINER_NAME" \
      --hostname zlo-linux-second \
      -p "127.0.0.1:$port:$port" \
      -v "$REPO_ROOT":/src:ro \
      -v "$TARGET_VOLUME":/build:ro \
      -v "$DATA_VOLUME":/data \
      -e ZLO_HOME=/data \
      -e HOME=/data/home \
      -e XDG_CONFIG_HOME=/data/home/.config \
      -e XDG_DATA_HOME=/data/home/.local/share \
      -e ZLO_SECOND_PORT="$port" \
      "$LINUX_IMAGE" \
      sh -eu -c '
        mkdir -p /data/home /data/local-dev /data/workspace
        chmod 700 /data /data/home /data/local-dev /data/workspace
        key_file="/data/local-dev/.reborn-local-dev-secrets-master-key"
        if [ ! -f "$key_file" ]; then
          umask 077
          od -An -tx1 -N32 /dev/urandom | tr -d " \n" >"$key_file"
          printf "\n" >>"$key_file"
          chmod 600 "$key_file"
        fi
        cat >/data/config.toml <<CFG
api_version = "ironclaw.runtime/v1"

[boot]
profile = "local-dev"

[webui]
listen_host = "127.0.0.1"
listen_port = $ZLO_SECOND_PORT
CFG
        eth_ip=$(ip -4 -o addr show scope global | awk "NR==1 {split(\$4, a, \"/\"); print a[1]}")
        if [ -z "$eth_ip" ]; then
          eth_ip=$(hostname -i | awk "{print \$1}")
        fi
        socat "TCP-LISTEN:$ZLO_SECOND_PORT,bind=$eth_ip,fork,reuseaddr" "TCP:127.0.0.1:$ZLO_SECOND_PORT" &
        cd /data/workspace
        exec /build/debug/zlo serve --host 127.0.0.1 --port "$ZLO_SECOND_PORT" --create-token-if-missing
      ' >/dev/null

    wait_for_descriptor "$port" "linux"
  fi

  cmd_status
}

cmd_status() {
  mode=$(current_mode)
  port=$(current_port)
  url="http://127.0.0.1:$port"

  if ! is_running; then
    printf 'status: stopped\n'
    printf 'mode:   %s\n' "$mode"
    printf 'url:    %s\n' "$url"
    printf 'data:   %s\n' "$DATA_DIR"
    return 1
  fi

  body=$(curl --silent --show-error --fail --max-time 5 "$url$DESCRIPTOR_PATH") || \
    die "backend process is running but descriptor failed at $url$DESCRIPTOR_PATH"

  env_id=$(printf '%s' "$body" | json_field environment_id)
  label=$(printf '%s' "$body" | json_field label)
  proto=$(printf '%s' "$body" | json_field protocol_version)

  printf 'status:         running\n'
  printf 'mode:           %s\n' "$mode"
  printf 'url:            %s\n' "$url"
  printf 'environment_id: %s\n' "$env_id"
  printf 'label:          %s\n' "$label"
  printf 'protocol:       %s\n' "$proto"
  if [ "$mode" = "native" ]; then
    printf 'pid:            %s\n' "$(cat "$PID_FILE")"
    printf 'data:           %s\n' "$DATA_DIR"
    printf 'log:            %s\n' "$LOG_FILE"
  else
    printf 'container:      %s\n' "$CONTAINER_NAME"
    printf 'volume:         %s\n' "$DATA_VOLUME"
  fi
}

cmd_pair() {
  mode=$(current_mode)
  if ! is_running; then
    die "second backend is not running; start it first"
  fi

  if [ "$mode" = "native" ]; then
    [ -x "$ZLO_BIN" ] || die "zlo binary not found at $ZLO_BIN"
    pair_json=$(run_native_zlo pair --json)
  else
    ensure_docker
    pair_json=$(docker exec \
      -e ZLO_HOME=/data \
      -e HOME=/data/home \
      -e XDG_CONFIG_HOME=/data/home/.config \
      -e XDG_DATA_HOME=/data/home/.local/share \
      "$CONTAINER_NAME" \
      /build/debug/zlo pair --json)
  fi

  code=$(printf '%s' "$pair_json" | json_field code)
  expires_at=$(printf '%s' "$pair_json" | json_field expires_at)
  link=$(printf '%s' "$pair_json" | json_field link)

  [ -n "$code" ] && [ -n "$link" ] || die "unexpected output from zlo pair --json"

  printf 'Pairing code: %s\n' "$code"
  printf 'Expires:      %s (single use)\n' "$expires_at"
  printf 'Link:         %s\n' "$link"
  printf '\nPaste the Link into ZLO -> Settings -> Computers -> Pairing link or code, then click Pair.\n'
}

read_backend_token() {
  mode="$1"
  if [ "$mode" = "native" ]; then
    token_file="$DATA_DIR/webui-token"
    [ -r "$token_file" ] || die "token file not found at $token_file"
    tr -d '\r\n' <"$token_file"
  else
    ensure_docker
    docker exec "$CONTAINER_NAME" cat /data/webui-token | tr -d '\r\n'
  fi
}

cmd_smoke() {
  mode=$(current_mode)
  port=$(current_port)
  url="http://127.0.0.1:$port"

  if ! is_running; then
    die "second backend is not running"
  fi

  # 1. Public descriptor must return 200 with environment_id and label.
  desc_tmp=$(mktemp)
  trap 'rm -f "$desc_tmp"' EXIT INT TERM
  desc_code=$(curl --silent --output "$desc_tmp" --write-out '%{http_code}' \
    --max-time 5 "$url$DESCRIPTOR_PATH")
  if [ "$desc_code" != "200" ]; then
    rm -f "$desc_tmp"
    die "descriptor check failed: expected HTTP 200, got $desc_code"
  fi
  env_id=$(json_field environment_id <"$desc_tmp")
  label=$(json_field label <"$desc_tmp")
  rm -f "$desc_tmp"
  trap - EXIT INT TERM
  if [ -z "$env_id" ] || [ -z "$label" ]; then
    die "descriptor check failed: missing environment_id or label"
  fi
  printf 'ok: descriptor 200 (environment_id=%s, label=%s)\n' "$env_id" "$label"

  # 2. Unauthenticated inbox call must be refused with 401.
  unauth_code=$(curl --silent --output /dev/null --write-out '%{http_code}' \
    --max-time 5 "$url$INBOX_PATH")
  if [ "$unauth_code" != "401" ]; then
    die "unauthenticated inbox check failed: expected HTTP 401, got $unauth_code"
  fi
  printf 'ok: unauthenticated %s -> 401\n' "$INBOX_PATH"

  # 3. Authenticated inbox call with the backend's own token must return 200.
  # Pass the Authorization header via curl config on stdin so the token never
  # appears in argv or stdout.
  token=$(read_backend_token "$mode")
  [ -n "$token" ] || die "empty webui-token"
  auth_code=$(printf 'header = "Authorization: Bearer %s"\n' "$token" | \
    curl --silent --output /dev/null --write-out '%{http_code}' \
      --max-time 5 -K - "$url$INBOX_PATH")
  if [ "$auth_code" != "200" ]; then
    die "authenticated inbox check failed: expected HTTP 200, got $auth_code"
  fi
  printf 'ok: authenticated %s -> 200\n' "$INBOX_PATH"
}

cmd_stop() {
  stopped_any=0

  if [ -f "$PID_FILE" ]; then
    pid=$(cat "$PID_FILE" 2>/dev/null || true)
    case "$pid" in
      ""|*[!0-9]*) ;;
      *)
        if kill -0 "$pid" 2>/dev/null; then
          kill "$pid" 2>/dev/null || true
          waited=0
          while [ "$waited" -lt 20 ] && kill -0 "$pid" 2>/dev/null; do
            sleep 0.5
            waited=$((waited + 1))
          done
          if kill -0 "$pid" 2>/dev/null; then
            kill -9 "$pid" 2>/dev/null || true
          fi
          stopped_any=1
        fi
        ;;
    esac
    rm -f "$PID_FILE"
  fi

  if docker_available; then
    if docker inspect "$CONTAINER_NAME" >/dev/null 2>&1; then
      docker rm -f "$CONTAINER_NAME" >/dev/null
      stopped_any=1
    fi
  fi

  if [ "$stopped_any" -eq 1 ]; then
    printf 'stopped second backend\n'
  else
    printf 'second backend was not running\n'
  fi
}

cmd_clean() {
  cmd_stop
  if [ -d "$DATA_DIR" ]; then
    rm -rf "$DATA_DIR"
    printf 'removed %s\n' "$DATA_DIR"
  fi
  if docker_available; then
    if docker volume inspect "$DATA_VOLUME" >/dev/null 2>&1; then
      docker volume rm "$DATA_VOLUME" >/dev/null
      printf 'removed Docker volume %s\n' "$DATA_VOLUME"
    fi
  fi
}

validate_data_dir

subcmd="${1:-}"
[ -n "$subcmd" ] || { usage >&2; exit 2; }
shift

case "$subcmd" in
  start) cmd_start "$@" ;;
  status) cmd_status "$@" ;;
  smoke) cmd_smoke "$@" ;;
  pair) cmd_pair "$@" ;;
  stop) cmd_stop "$@" ;;
  clean) cmd_clean "$@" ;;
  -h|--help|help) usage ;;
  *)
    printf 'unknown command: %s\n' "$subcmd" >&2
    usage >&2
    exit 2
    ;;
esac
