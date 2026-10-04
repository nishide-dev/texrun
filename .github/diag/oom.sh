#!/bin/sh
# Temporary diagnostic (#60): kills containers with the OOM killer, W
# workers in parallel, N each, and reads Docker's `OOMKilled` right after
# the exit, after 1 s and after 5 s, plus the cgroup's `oom_kill` counter
# read by a surviving shell in the container.
image=$1; workers=$2; n=$3
work() {
  w=$1; i=0
  while [ $i -lt $n ]; do
    i=$((i+1)); c=oomdiag-$w-$i
    docker create -q --init --memory 32m --memory-swap 32m --pids-limit 32 --cpus 1 --name $c --entrypoint /bin/sh "$image" -c '
      sh -c "x=\$(head -c 134217728 /dev/zero | tr \"\\0\" a); echo \${#x}"
      while read -r k v; do [ "$k" = oom_kill ] && echo "counter=$v" >&2; done < /sys/fs/cgroup/memory.events
      exit 137' >/dev/null
    counter=$(docker start -a $c 2>&1 >/dev/null | sed -n 's/^counter=//p')
    a=$(docker inspect -f '{{.State.ExitCode}} {{.State.OOMKilled}}' $c)
    sleep 1; b=$(docker inspect -f '{{.State.OOMKilled}}' $c)
    sleep 4; d=$(docker inspect -f '{{.State.OOMKilled}}' $c)
    echo "w$w#$i exit+flag=$a 1s=$b 5s=$d counter=$counter"
    docker rm -f $c >/dev/null
    # the same with the main process itself killed (like the test)
    c=oomdiag-main-$w-$i
    docker create -q --init --memory 32m --memory-swap 32m --pids-limit 32 --cpus 1 --name $c --entrypoint /bin/sh "$image" -c 'x=$(head -c 134217728 /dev/zero | tr "\0" a); echo ${#x}' >/dev/null
    docker start -a $c >/dev/null 2>&1
    a=$(docker inspect -f '{{.State.ExitCode}} {{.State.OOMKilled}}' $c)
    sleep 1; b=$(docker inspect -f '{{.State.OOMKilled}}' $c)
    sleep 4; d=$(docker inspect -f '{{.State.OOMKilled}}' $c)
    echo "w$w#$i main exit+flag=$a 1s=$b 5s=$d"
    docker rm -f $c >/dev/null
  done
}
k=0
while [ $k -lt $workers ]; do k=$((k+1)); work $k & done
wait
