---
name: perf
description: Run tq's performance regression bench (glass-to-glass latency per hop, clock-sync accuracy, teleop and remote-stop round trips, data tracks, fan-out) and compare against a baseline. Use when asked to benchmark, check for latency regressions, measure performance, or compare a branch or commit against another.
---

# tq performance bench

The bench is `dev/bench/bench.ts`; [docs/perf.md](../../../docs/perf.md)
explains each scenario, what the numbers mean, and what they leave out.
Read it before reporting results.

## Run

```bash
bun dev/bench/bench.ts list                       # scenarios; * = in --quick
bun dev/bench/bench.ts run --quick                # ~4 min, the key scenarios
bun dev/bench/bench.ts run                        # everything, ~8 min
bun dev/bench/bench.ts run --only video-direct,data
bun dev/bench/bench.ts run --app                  # also tq-app decode→paint (opens a window)
bun dev/bench/bench.ts run --ref main             # build another ref in a worktree, bench that
bun dev/bench/bench.ts compare BASE.json NEW.json [--fail]
bun dev/bench/bench.ts compare NEW.json           # against the committed baseline (last release)
# against the deployed GCP relay (start the VM first if it is stopped):
TQ_SHARED_SECRET=$(terraform -chdir=infra/gcp output -raw shared_secret) \
  bun dev/bench/bench.ts run --relay https://tq-relay-1.haffi.dev
```

Results land in `target/bench/<time>-<label>.json`, with every process's
log in the `-logs` directory next to it.

## Checking a change for regressions

1. Stop anything on the bench ports (QUIC 14443, HTTP 18080, 14444,
   15004/15010/15011, 16000–16002). A `dev/run-local.sh` stack on the
   default ports can keep running, but it shares the CPU and encoder, so
   stop it for numbers you will compare.
2. Bench the baseline and the change **on the same machine, interleaved**,
   at least 2 rounds each: `--ref <base>` for the baseline, the working tree
   for the change. One run of each is not enough; the source encoder and
   the impairment proxy are noisy (see "Noise" in docs/perf.md).
3. `compare` the pairs. A REGRESSED line is past both a relative and an
   absolute margin. If it does not repeat across rounds, it is noise.
   If it repeats, look at the hop that moved (`capture→pub` = tq-edge or
   the encoder, `pub→relay` = uplink, `relay→probe` = relay and downlink).
4. Check `clock_check`: on one host the corrected numbers must stay within
   the claimed bound (`within_bound` ≈ 100%). If not, the latency numbers
   themselves are wrong; fix that first.
5. Report the comparison table and the hop that explains any change.
   Say which numbers are from emulated links.

## Quick check against the last release

`bun dev/bench/bench.ts run --quick`, then `compare` with only the new
result. It uses `dev/bench/baselines/<machine>/<relay>.json`. If there is
no baseline for this machine, say so and fall back to `--ref <last
release tag>`, interleaved as above. A flag against a baseline is a
single-run comparison: confirm it before reporting it as a regression.

## On a release

From a clean checkout of the release commit, make a full run with
`--app --save-baseline`, and also with `--relay https://tq-relay-1.haffi.dev`
once the release's relay is deployed (the VM is normally stopped: ask
before starting it). Commit the baselines with the release.

## When the code changes under the bench

The project moves fast. If a scenario fails, read its logs first:
- A tq-edge config field changed: update `edgeConfig()`.
- tq-probe's JSON changed: update `videoTracks()`/`metrics()`.
- A new feature worth tracking: add a scenario to `SCENARIOS` and its
  metrics to `metrics()`, and document it in docs/perf.md.
Keep scenario names stable; `compare` matches metrics by name.

Protocol 2.0 builds (`tq-publisher`, before tq-edge) are benched through
the `legacy` paths in `edgeConfig()` and `probe()`. Delete those once no
baseline worth comparing against predates protocol 3.0.
