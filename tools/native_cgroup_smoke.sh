#!/usr/bin/env bash
set -euo pipefail
umask 077

[[ "$(uname -s)" == Linux ]] || { echo "solo Linux" >&2; exit 2; }
ROOT="${SYSOPT_TEST_CGROUP_ROOT:-}"
[[ -n "$ROOT" ]] || { echo "SYSOPT_TEST_CGROUP_ROOT debe apuntar a una raíz cgroup v2 delegada" >&2; exit 2; }
[[ "$ROOT" == /sys/fs/cgroup/* && -d "$ROOT" && ! -L "$ROOT" ]] || {
  echo "raíz cgroup insegura o inexistente: $ROOT" >&2; exit 2;
}
[[ -f "$ROOT/cgroup.controllers" && -f "$ROOT/cgroup.procs" ]] || {
  echo "la raíz no es cgroup v2" >&2; exit 2;
}
[[ -w "$ROOT/cgroup.subtree_control" ]] || { echo "raíz no delegada/escribible" >&2; exit 2; }

NAME="sysopt-smoke-${GITHUB_RUN_ID:-local}-$$"
GROUP="$ROOT/$NAME"
PID=""
ORIGINAL_REL=""
cleanup() {
  set +e
  if [[ -n "$PID" && -d "/proc/$PID" ]]; then
    if [[ -n "$ORIGINAL_REL" && -w "/sys/fs/cgroup${ORIGINAL_REL}/cgroup.procs" ]]; then
      echo "$PID" > "/sys/fs/cgroup${ORIGINAL_REL}/cgroup.procs"
    fi
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  rmdir "$GROUP" 2>/dev/null || true
}
trap cleanup EXIT

controllers="$(cat "$ROOT/cgroup.controllers")"
for controller in cpu io memory; do
  if grep -qw "$controller" <<<"$controllers"; then
    printf '+%s\n' "$controller" > "$ROOT/cgroup.subtree_control" 2>/dev/null || true
  fi
done
mkdir "$GROUP"
[[ ! -L "$GROUP" && -f "$GROUP/cgroup.procs" ]] || { echo "grupo creado inválido" >&2; exit 1; }

sleep 60 & PID=$!
ORIGINAL_REL="$(awk -F: '$1=="0" {print $3; exit}' "/proc/$PID/cgroup")"
[[ -n "$ORIGINAL_REL" ]] || { echo "no se pudo resolver cgroup original" >&2; exit 1; }
echo "$PID" > "$GROUP/cgroup.procs"
grep -qx "$PID" "$GROUP/cgroup.procs"

if [[ -w "$GROUP/cpu.weight" ]]; then
  echo 321 > "$GROUP/cpu.weight"
  [[ "$(cat "$GROUP/cpu.weight")" == 321 ]]
fi
if [[ -w "$GROUP/io.weight" ]]; then
  echo 'default 321' > "$GROUP/io.weight"
  grep -Eq '(^|[[:space:]])321($|[[:space:]])' "$GROUP/io.weight"
fi
if [[ -w "$GROUP/memory.high" ]]; then
  echo 67108864 > "$GROUP/memory.high"
  [[ "$(cat "$GROUP/memory.high")" == 67108864 ]]
fi

echo "native cgroup v2 smoke: PASS"
