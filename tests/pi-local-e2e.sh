#!/usr/bin/env bash
# Python owns the isolated runtime so this wrapper never changes its own HOME or XDG state.
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
engine=${VHRN_ENGINE:-${ENGINE:-}}
docker_variant=${DOCKER_VARIANT:-}
case "${1:-}" in
  --self-test) exec python3 "$repo/tests/fake-pi-provider.py" --self-test ;;
  --case) [[ $# -eq 2 ]] || { printf 'usage: %s [--self-test|--case smoke|tool|policy|cancel|endpoints|signals|sessions|persistence|trust|remote-openai|remote-anthropic|--remote-openai|--remote-anthropic|all]\nremote cases require VHRN_PI_REMOTE_<OPENAI|ANTHROPIC>_{URL,MODEL,KEY}.\n' "$0" >&2; exit 2; }; case_name=$2 ;;
  --remote-openai) [[ $# -eq 1 ]] || exit 2; case_name=remote-openai ;;
  --remote-anthropic) [[ $# -eq 1 ]] || exit 2; case_name=remote-anthropic ;;
  "") case_name=all ;;
  *) printf 'usage: %s [--self-test|--case smoke|tool|policy|cancel|endpoints|signals|sessions|persistence|trust|remote-openai|remote-anthropic|--remote-openai|--remote-anthropic|all]\n' "$0" >&2; exit 2 ;;
esac
[[ "$engine" == container || "$engine" == docker ]] || { printf 'set VHRN_ENGINE to container or docker\n' >&2; exit 2; }
if [[ "$engine" == docker && "$docker_variant" != colima ]]; then
  printf 'Docker coverage requires DOCKER_VARIANT=colima\n' >&2
  exit 2
fi
if [[ "${case_name:-all}" == all ]]; then
  for test_case in smoke tool policy cancel endpoints signals sessions persistence trust; do
    python3 "$repo/tests/fake-pi-provider.py" --runtime "$engine" --case "$test_case"
  done
  exit 0
fi
exec python3 "$repo/tests/fake-pi-provider.py" --runtime "$engine" --case "$case_name"
