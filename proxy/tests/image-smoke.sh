#!/usr/bin/env bash
set -euo pipefail

# Docker: ENGINE=docker IMAGE=registry.example/vhrn-proxy:tag HOST_PORT=18080 ./image-smoke.sh
# Apple: ENGINE=container IMAGE=registry.example/vhrn-proxy:tag ./image-smoke.sh
: "${ENGINE:?set ENGINE to docker or container}"
: "${IMAGE:?set IMAGE to the image under test}"
case "$ENGINE" in
  docker|container) ;;
  *) echo "unsupported ENGINE: $ENGINE" >&2; exit 2 ;;
esac
if [ "$ENGINE" = docker ]; then
  : "${HOST_PORT:?set HOST_PORT to an unused numeric loopback port for Docker}"
  case "$HOST_PORT" in
    *[!0-9]*|'') echo "HOST_PORT must be numeric" >&2; exit 2 ;;
  esac
  host_port_number=$((10#$HOST_PORT))
  if (( host_port_number < 1 || host_port_number > 65535 )); then
    echo "HOST_PORT must be in 1..65535" >&2
    exit 2
  fi
fi
for command in "$ENGINE" curl tr sed grep mktemp tail; do
  command -v "$command" >/dev/null 2>&1 || { echo "required command unavailable: $command" >&2; exit 2; }
done

workdir=$(mktemp -d "${TMPDIR:-/tmp}/vhrn-proxy-smoke.XXXXXX")
case "$workdir" in
  "${TMPDIR:-/tmp}"/vhrn-proxy-smoke.*) ;;
  *) echo "unexpected temporary path: $workdir" >&2; exit 2 ;;
esac
container_name="vhrn-proxy-smoke-${workdir##*.}"
cleanup_eligible=false
cleanup() {
  if [ "$cleanup_eligible" = true ]; then
    if [ "$ENGINE" = docker ]; then
      "$ENGINE" rm -f "$container_name" >/dev/null 2>&1 || true
    else
      "$ENGINE" delete --force "$container_name" >/dev/null 2>&1 || true
    fi
  fi
  case "$workdir" in
    "${TMPDIR:-/tmp}"/vhrn-proxy-smoke.*) rm -rf "$workdir" ;;
  esac
}
trap cleanup EXIT INT TERM

run_with_timeout() {
  local seconds=$1
  local ticks=0
  local maximum_ticks=$((seconds * 10))
  shift
  "$@" &
  local pid=$!
  while kill -0 "$pid" >/dev/null 2>&1; do
    if (( ticks >= maximum_ticks )); then
      kill "$pid" >/dev/null 2>&1 || true
      wait "$pid" || true
      return 124
    fi
    sleep 0.1
    ticks=$((ticks + 1))
  done
  wait "$pid"
}

valid_ipv4() {
  local address=$1
  local octet
  local -a octets
  [[ "$address" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || return 1
  IFS=. read -r -a octets <<<"$address"
  for octet in "${octets[@]}"; do
    (( 10#$octet <= 255 )) || return 1
  done
}

emit_runtime_logs() {
  run_with_timeout 2 "$ENGINE" logs "$container_name" 2>&1 | tail -n 100 >&2 || true
}

printf 'allowed.example\n' >"$workdir/allowlist"
printf 'enforce\n' >"$workdir/mode"
mkdir "$workdir/log"
inspect_json=$("$ENGINE" image inspect "$IMAGE")
inspect_compact=$(printf '%s' "$inspect_json" | tr -d '[:space:]' | sed 's#\\/#/#g')
printf '%s' "$inspect_compact" | grep -F '"User":"65532:65532"' >/dev/null || { echo "image must declare user 65532:65532" >&2; exit 1; }
printf '%s' "$inspect_compact" | grep -F '"Entrypoint":["/vhrn-proxy"]' >/dev/null || { echo "image must declare the /vhrn-proxy entrypoint" >&2; exit 1; }
if [ "$ENGINE" = docker ]; then
  printf '%s' "$inspect_compact" | grep -F '"ExposedPorts":{"8080/tcp":' >/dev/null || { echo "Docker image must expose 8080/tcp" >&2; exit 1; }
else
  printf '%s' "$inspect_compact" | grep -F '"created_by":"EXPOSE[8080/tcp]"' >/dev/null || { echo "Apple image history must expose 8080/tcp" >&2; exit 1; }
fi

cleanup_eligible=true
run_arguments=(-d --rm --name "$container_name")
if [ "$ENGINE" = docker ]; then
  run_arguments+=(-p "127.0.0.1:${HOST_PORT}:8080")
fi
run_arguments+=(
  -v "$workdir/allowlist:/etc/vhrn/allowlist:ro" \
  -v "$workdir/mode:/etc/vhrn/mode:ro" \
  -v "$workdir/log:/var/log/vhrn" \
  -e VHRN_DENY_LOG=/var/log/vhrn/deny.log \
  "$IMAGE"
)
if ! run_with_timeout 10 "$ENGINE" run "${run_arguments[@]}" >/dev/null; then
  emit_runtime_logs
  echo "proxy container failed to start" >&2
  exit 1
fi

attempt=0
container_ip=
while (( attempt < 10 )); do
  if [ "$ENGINE" = docker ]; then
    endpoint="http://127.0.0.1:${HOST_PORT}/healthz"
  else
    inspect_output=$(run_with_timeout 2 "$ENGINE" inspect "$container_name" 2>/dev/null || true)
    inspect_line=$(printf '%s\n' "$inspect_output" | grep -m 1 'ipv4Address' || true)
    container_ip=$(printf '%s\n' "$inspect_line" | grep -Eo '[0-9]{1,3}(\.[0-9]{1,3}){3}' | sed -n '1p')
    if ! valid_ipv4 "$container_ip"; then
      attempt=$((attempt + 1))
      sleep 0.1
      continue
    fi
    endpoint="http://${container_ip}:8080/healthz"
  fi
  if response=$(run_with_timeout 1 curl -fsS "$endpoint" 2>/dev/null); then
    [ "$response" = ok ] || { echo "unexpected health response" >&2; exit 1; }
    exit 0
  fi
  attempt=$((attempt + 1))
  sleep 0.1
done
emit_runtime_logs
echo "proxy never became healthy" >&2
exit 1
