# Composition performance protocol v3

Historical protocol. The runner now uses [v4](composition-protocol-v4.md), which raises the measurement-host minimum to macOS 27 while retaining these host-load limits. The commands below describe the superseded v3 procedure.

This version changes only the host idle limits in [protocol v2](composition-protocol-v2.md). The twenty-block order and confidence bound stay unchanged. The reference revision, 33 absolute budgets, and 1.10 relative limit also stay unchanged. V3 reuses the seed and order schedule. The evaluator checks the hash of [the v3 manifest](composition-protocol-v3.json) before measurement.

## Host-control correction

The v2 limits rejected ordinary desktop activity before measurement. They permitted 5% whole-host CPU use and a one-minute load of 2.0. On 2026-09-08, `top -l 4 -s 2 -n 0` showed 88.10%, 88.56%, and 89.23% CPU idle. One-minute load fell from 5.04 to 4.72. The v2 A/A attempt then exhausted its five-minute initial idle window. It did not build or measure. These observations show that the old gate was impractical. They do not establish benchmark stability.

V3 permits at most 20% whole-host CPU use and a one-minute load of 6.0. These limits leave margin over the observed 11–12% CPU use and 4.72–5.04 load without admitting the substantially busier 35%/8.0 envelope. At the CPU boundary, at least 80% aggregate capacity remains idle. Neither measure proves core placement or absence of interference. Seven consecutive checks remain necessary, with five seconds between checks. Per-process checks, competitor exclusion, AC power, and nominal thermal state still apply. The runner retains failed samples and runs. It does not retry or remove measured observations.

On 2026-09-23, an unrelated Go test ran while samples showed load 16–26 and CPU idle as low as 0%. V3 rejects this activity. No benchmark result informed either new limit.

## Qualification order

First, run `reference-reference-aa` under v3 against two independent builds of pinned reference `b9eb6e732d2b0cf6ce3cd7255a5958781587af6c`. Require every relative A/A gate and all 33 reference absolute budgets to pass. Then run `candidate-reference` at a new output path, supplying the published A/A directory with `--aa-evidence`. The runner checks its protocol, plan, checkout revision, host identity, and script hashes, then re-evaluates the A/A raw measurements against all relative and absolute gates before candidate measurement. It pins the receipt hashes until publication. A/A passing supplies evidence for the host policy. Compare candidate telemetry with A/A telemetry before accepting a candidate result; A/A does not validate conditions outside its observed range. The chosen limits alone supply no proof. If A/A fails, retain its evidence. Investigate the reference result before changing any policy or budget. Neither old v2 attempt produced benchmark observations.

Use distinct new paths below `target/qualification-evidence/0.9.0-alpha.1/composition`:

```sh
scripts/qualify-composition-performance.sh --mode reference-reference-aa <aa-output> <pinned-reference-worktree>
scripts/qualify-composition-performance.sh --mode candidate-reference --aa-evidence <aa-output> <candidate-output> <pinned-reference-worktree>
```
