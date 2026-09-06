#!/usr/bin/env bash
# Verify the engine route a host-side loopback broker would use.
set -euo pipefail

engine=${ENGINE:-}
network=${NETWORK:-}
image=${PROBE_IMAGE:-alpine:3.22}
docker_variant=${DOCKER_VARIANT:-}
workdir=$(mktemp -d "${TMPDIR:-/tmp}/vhrn-loopback.XXXXXX")
ready_file="$workdir/ready"
server_log="$workdir/server.log"
listener_pid=
probe_name="vhrn-loopback-probe-$$"

cleanup() {
  case "$engine" in
    container) container delete --force "$probe_name" >/dev/null 2>&1 || true ;;
    docker) docker rm --force "$probe_name" >/dev/null 2>&1 || true ;;
  esac
  if [[ -n "$listener_pid" ]]; then
    kill "$listener_pid" 2>/dev/null || true
    wait "$listener_pid" 2>/dev/null || true
  fi
  rm -rf "$workdir"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

die() {
  printf 'vhrn loopback probe: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "need $1"
}

ipv4_gateway() {
  python3 -c '
import json
import re
import sys

try:
    value = json.load(sys.stdin)
except json.JSONDecodeError as error:
    raise SystemExit(f"could not parse network inspection JSON: {error}")

gateways = []
def visit(node):
    if isinstance(node, dict):
        for key, child in node.items():
            if key.lower() in ("gateway", "ipv4gateway") and isinstance(child, str):
                gateways.append(child)
            visit(child)
    elif isinstance(node, list):
        for child in node:
            visit(child)

visit(value)
for gateway in gateways:
    match = re.fullmatch(r"(\d{1,3}(?:\.\d{1,3}){3})(?:/\d{1,2})?", gateway)
    if match:
        octets = match.group(1).split(".")
        if all(int(octet) <= 255 for octet in octets):
            print(match.group(1))
            break
else:
    raise SystemExit("network inspection did not contain an IPv4 gateway")
'
}

lan_address() {
  local interface address
  interface=$(route -n get default 2>/dev/null | awk '/interface:/{print $2; exit}') || true
  if [[ -n "$interface" ]]; then
    address=$(ipconfig getifaddr "$interface" 2>/dev/null || true)
    if [[ "$address" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]]; then
      printf '%s\n' "$address"
      return 0
    fi
  fi
  return 1
}

check_lan_blocked() {
  local address=$1 port=$2
  if python3 - "$address" "$port" <<'PY'
import socket
import sys

try:
    connection = socket.create_connection((sys.argv[1], int(sys.argv[2])), timeout=1)
except OSError:
    raise SystemExit(0)
else:
    connection.close()
    raise SystemExit(1)
PY
  then
    printf 'lan-reachability: blocked (%s:%s)\n' "$address" "$port"
  else
    die "listener was reachable through LAN address $address:$port"
  fi
}

[[ -n "$engine" ]] || die "set ENGINE=container or ENGINE=docker"
require_command python3
require_command "$engine"

case "$engine" in
  container)
    [[ -z "$docker_variant" ]] || die "DOCKER_VARIANT only applies to ENGINE=docker"
    require_command route
    require_command ipconfig
    network=${network:-default}
    engine_version=$(container --version)
    network_inspect=$(container network inspect "$network")
    bind_address=$(printf '%s' "$network_inspect" | ipv4_gateway)
    dial_host=$bind_address
    run_command=(container run --rm --name "$probe_name" --network "$network" "$image")
    ;;
  docker)
    [[ "$docker_variant" == colima ]] || die "ENGINE=docker requires DOCKER_VARIANT=colima"
    require_command route
    require_command ipconfig
    network=${network:-bridge}
    engine_version=$(docker version --format 'client={{.Client.Version}} server={{.Server.Version}}')
    if [[ -n ${DOCKER_CONTEXT:-} ]]; then
      docker_context=$DOCKER_CONTEXT
      effective_host=$(docker context inspect "$docker_context" --format '{{.Endpoints.docker.Host}}')
      docker_selection=DOCKER_CONTEXT
    elif [[ -n ${DOCKER_HOST:-} ]]; then
      docker_context=$(docker context show)
      effective_host=$DOCKER_HOST
      docker_selection=DOCKER_HOST
    else
      docker_context=$(docker context show)
      effective_host=$(docker context inspect "$docker_context" --format '{{.Endpoints.docker.Host}}')
      docker_selection=context
    fi
    daemon_name=$(docker info --format '{{.Name}}')
    [[ "$effective_host" == unix://* ]] || die "local broker probing requires a local Unix Docker socket, got $effective_host"
    if [[ "$effective_host" != *".colima/"* && "$docker_context" != *colima* && "$daemon_name" != *colima* ]]; then
      die "effective Docker daemon is not recognizably Colima (context=$docker_context host=$effective_host name=$daemon_name)"
    fi
    network_inspect=$(docker network inspect "$network")
    bind_address=127.0.0.1
    dial_host=host.docker.internal
    run_command=(docker run --rm --name "$probe_name" --network "$network" "$image")
    ;;
  *) die "ENGINE must be container or docker, got $engine" ;;
esac

lan=$(lan_address) || die "could not determine the default-route LAN IPv4 address"

printf 'engine: %s\n' "$engine"
printf 'engine-version: %s\n' "$engine_version"
printf 'network: %s\n' "$network"
printf 'network-inspect: %s\n' "$(printf '%s' "$network_inspect" | tr '\n' ' ')"
if [[ "$engine" == docker ]]; then
  printf 'docker-context: %s\n' "$docker_context"
  printf 'docker-selection: %s\n' "$docker_selection"
  printf 'docker-host: %s\n' "$effective_host"
  printf 'docker-daemon: %s\n' "$daemon_name"
fi
printf 'bind-address: %s\n' "$bind_address"
printf 'proxy-dial: %s:<assigned-port>\n' "$dial_host"
printf 'lan-address: %s\n' "$lan"

python3 - "$bind_address" "$ready_file" <<'PY' >"$server_log" 2>&1 &
import http.server
import socket
import sys

bind_address, ready_file = sys.argv[1:]

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", "3")
        self.end_headers()
        self.wfile.write(b"ok\n")

    def log_message(self, _format, *_args):
        pass

class Server(http.server.HTTPServer):
    address_family = socket.AF_INET

server = Server((bind_address, 0), Handler)
with open(ready_file, "w", encoding="ascii") as ready:
    ready.write(str(server.server_port))
server.serve_forever()
PY
listener_pid=$!

for _ in {1..50}; do
  [[ -s "$ready_file" ]] && break
  if ! kill -0 "$listener_pid" 2>/dev/null; then
    cat "$server_log" >&2 || true
    die "host listener exited before reporting readiness"
  fi
  sleep 0.1
done
[[ -s "$ready_file" ]] || die "host listener did not report readiness"
port=$(<"$ready_file")
[[ "$port" =~ ^[0-9]+$ ]] || die "host listener returned an invalid port"

printf 'bind: %s:%s\n' "$bind_address" "$port"
printf 'proxy-dial: %s:%s\n' "$dial_host" "$port"

# shellcheck disable=SC2016 # $1 expands in the probe container, not the host shell.
"${run_command[@]}" sh -ceu '
  wget -T 5 -t 1 -q -O - "$1" | grep -qx ok
' probe "http://$dial_host:$port/probe"
printf 'container-reachability: passed (%s:%s)\n' "$dial_host" "$port"

check_lan_blocked "$lan" "$port"
printf 'result: passed\n'
