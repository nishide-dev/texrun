#!/bin/sh
# Runs a command with a delegated cgroup for texrun (docs/development.md,
# "cgroup tests"). For a `docker run --privileged` container of the dev
# image only: the cgroup mount is read-only otherwise.
#
# cgroup v2 hands controllers to child cgroups only from a cgroup without
# processes of its own, so every process of the container is first moved
# from the namespace root into a leaf (`init`). texrun then creates its
# per-run cgroups directly under the namespace root (Cgroups::detect).
set -eu
cg=/sys/fs/cgroup
if [ ! -w "$cg/cgroup.procs" ]; then
  echo "with-cgroup.sh: $cg is not writable (run the container with --privileged)" >&2
  exit 1
fi
mkdir -p "$cg/init"
for pid in $(cat "$cg/cgroup.procs"); do
  # Kernel threads and processes that are gone cannot (and need not) move.
  echo "$pid" > "$cg/init/cgroup.procs" 2>/dev/null || true
done
echo "+memory +pids +cpu" > "$cg/cgroup.subtree_control"
export TEXRUN_REQUIRE_CGROUP=1
exec "$@"
