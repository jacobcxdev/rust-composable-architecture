# Darwin command resource accounting

This is an instrumentation diagnostic, not RCA performance evidence. It informs
the outstanding clean-check and clean-release comparison programme; it does not
freeze a new timing protocol or qualify the host for timing.

## Meaning and limits

Local `time(1)`, `getrusage(2)` and `wait(2)` documentation describes `time -l`
as reporting resource usage, with CPU accounting including waited-for children.
Darwin reports maximum resident set size in bytes. The maximum RSS counter is
not summed simultaneous process-tree memory. Cargo and rustc can coexist even
with `-j 1`; do not label this counter aggregate peak compiler memory.

Use these labels:

- Command elapsed wall seconds.
- Command plus reaped descendants' user and system CPU seconds.
- Maximum reported process RSS across the accounted command and descendants.

Detached workers, remote compilers and cache daemons are not covered by this
diagnostic. The separate `peak memory footprint` counter must not be substituted
for aggregate build memory. Neither counter identifies which crate caused a cost.

`/usr/bin/time -p` prints two decimal places on this host. The diagnostic
`/usr/bin/time -lp /usr/bin/true` printed zero for real, user and system time.
Printed precision is not accuracy. Preserve zeros and disable multiplicative
analysis for unresolved zero-valued observations; do not add pseudocounts.
Retain user/system separately even when also reporting their sum.

## Native control

On macOS 27.0 (`26A428`), a parent started two concurrent Python children,
each touching a 32 MiB allocation and consuming about 0.15 CPU seconds. It
waited for both. An independent first run was retained only in the task
transcript; the parent repeated it and saved fresh evidence at
`/tmp/rca-darwin-time-descendants-20260930.log`.

The saved run reported parent CPU 0.028944 seconds and accumulated child CPU
0.341939 seconds. External `time` reported user 0.16 and system 0.21 seconds.
Parent maximum RSS was 10,846,208 bytes; accumulated child maximum RSS and
external `time` both reported 43,532,288 bytes, not the sum of the two children.
The distinct peak-footprint counter was 7,291,240 bytes.

- Saved output SHA-256: `6e59ee3a4275ae9e7d3ab4128de5e7bfdf9f134b163492cacfb71365b58c1c09`.
- `/usr/bin/time` SHA-256: `6fe69a2da5af0a6e965785f8fbb03901c5bb8eef7f308f70620d4458ed28cce7`.

Reproduce the workload (values vary):

```sh
/usr/bin/time -lp /usr/bin/python3 -c 'import subprocess,sys,resource,json; code="import time,resource,json; x=bytearray(32*1024*1024); t=time.process_time();\nwhile time.process_time()-t<0.15: pass\nprint(json.dumps(dict(cpu=resource.getrusage(resource.RUSAGE_SELF).ru_utime+resource.getrusage(resource.RUSAGE_SELF).ru_stime,rss=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss)))"; ps=[subprocess.Popen([sys.executable,"-c",code],stdout=subprocess.PIPE,text=True) for _ in range(2)]; print([p.communicate()[0].strip() for p in ps]); print({str(k):dict(cpu=resource.getrusage(k).ru_utime+resource.getrusage(k).ru_stime,rss=resource.getrusage(k).ru_maxrss) for k in [resource.RUSAGE_SELF,resource.RUSAGE_CHILDREN]})'
```

Future compile capture should reuse the existing host monitor and parser, retain
raw counters bound to successful command receipts, and cross-check parsed rows.
Use fresh Cargo target directories with authenticated, already-fetched sources;
call this target-clean, not OS-cache-cold. Clean check and clean release are
separate workloads. Application-edit incremental checking requires its own
declared source edit and target history; it cannot replace clean-check evidence.
