#!/bin/bash
# TechEmpower-style load: run.sh [rounds]; services on CPUs 0-15, PostgreSQL 16-23, wrk 24-31.
# APPS="zell go dotnet", TESTS="db fortunes ...", C (connections, 256), D (seconds, 15).
# Binaries: /root/te-zell, /root/te-go, /root/te-dotnet-out/te-dotnet (see README.md).
R=${1:-3}; C=${C:-256}; D=${D:-15}
OUT=/root/results/te-$(date +%Y%m%d-%H%M); mkdir -p $OUT; echo $OUT
declare -A APP=([zell]=/root/te-zell [go]=/root/te-go [dotnet]=/root/te-dotnet-out/te-dotnet)
APPS=${APPS:-zell go dotnet}
TESTS=${TESTS:-"plaintext json db queries?queries=20 fortunes updates?queries=20"}
for r in $(seq $R); do
  for a in $APPS; do
    taskset -c 0-15 ${APP[$a]} >$OUT/$a.log 2>&1 &
    pid=$!; sleep 3
    for t in $TESTS; do
      taskset -c 24-31 wrk -t8 -c$C -d5s "http://127.0.0.1:8080/$t" >/dev/null 2>&1
      res=$(taskset -c 24-31 wrk -t8 -c$C -d${D}s --latency "http://127.0.0.1:8080/$t" 2>&1)
      rps=$(echo "$res" | awk '/Requests\/sec/{print $2}')
      p50=$(echo "$res" | awk '$1=="50%"{print $2}'); p99=$(echo "$res" | awk '$1=="99%"{print $2}')
      err=$(echo "$res" | awk '/Non-2xx|Socket errors/{printf "%s ", $0}')
      rss=$(awk '/VmHWM/{print $2}' /proc/$pid/status)
      echo "$r $a ${t%%\?*} $rps $p50 $p99 rss_kb=$rss $err" | tee -a $OUT/results.txt
    done
    kill $pid; wait $pid 2>/dev/null; sleep 1
  done
done
