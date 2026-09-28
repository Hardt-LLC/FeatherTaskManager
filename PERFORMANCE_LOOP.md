# Performance improvement loop

Keep each change small, measurable, and reviewable on a `codex/` branch. Preserve the
finished design. Run one bounded investigation at a time; the application does not
self-modify or run a permanent optimizer in the background.

## Capture and compare

Build the baseline before changing source, then capture it. Keep its executable and
capture locally until the candidate is reviewed. Do not reuse historical release
numbers as if they were measured under today's conditions.

```powershell
cargo build --release --locked
.\scripts\performance-loop.ps1 -Label 'Baseline: release build before the change'
```

The script prints the full path to a new `capture.json` under ignored
`target/performance-loop/<run-id>/`. Use that exact file explicitly for comparison:

```powershell
cargo build --release --locked
.\scripts\performance-loop.ps1 -BaselinePath '<baseline capture.json>' -Label 'Candidate: describe the change'
```

Default coverage is Processes and Performance, three launches each, with ten seconds
of sampling after up to ten seconds of startup observation. Use `-Pages services`
when changing the service collector, or `-Pages processes -DurationSeconds 5` for a
short focused run. Baseline and candidate must use the same parameters. The allowed
limits are 1–60 seconds per run and 3–9 repetitions. A capture lock prevents two
instances of this runner in the same checkout from competing. Do not run builds,
tests, or another benchmark concurrently with measurement.

To examine existing captures without launching the application:

```powershell
.\scripts\performance-loop.ps1 -BaselinePath '<baseline capture.json>' -CandidatePath '<candidate capture.json>'
```

Raw page reports, capture context, comparison results, and any error report are
saved in a new local directory. The runner never overwrites or promotes a baseline.
`measure.ps1` raw reports are insufficient as loop baselines because they lack the
machine, power, privilege, and preference context. Local reports include computer
names and paths; keep them out of commits and published releases.

## Meaningful regressions

The comparison uses the median of repeated runs and median absolute deviation
(MAD). A regression must exceed the largest of the relative allowance, the absolute
floor, and three times the larger baseline/candidate MAD.

| Metric | Relative allowance | Absolute floor |
| --- | ---: | ---: |
| Process CPU, normalized to the whole machine | 20% | 0.10 percentage points |
| Sampled maximum working set | 10% | 2 MiB |
| Sampled maximum private commit | 10% | 2 MiB |
| Sampled maximum handle count | 10% | 16 handles |
| Time to observed input idle | 20% | 30 ms |

`within-budget` means this short measurement found no material regression. It does
not prove an improvement. `improved` requires a reduction exceeding the same
threshold. Excessive variation is `noisy`, and missing metrics are `unavailable`;
both require review and produce a failing exit status, just like a regression.
Failed measurements or mismatched machine, OS, CPU, power plan, elevation,
preferences, measurement script, page, duration, or repetition count fail closed.
Settings are only read, and a change during capture invalidates the run.

Each capture records the binary hash/size, current checkout HEAD/branch/dirty state,
Rust compiler details, and a freeform label. Checkout/toolchain observations alone
do not establish which source produced an existing executable. State that caveat
when measuring a previously built binary. The executable hash must match across
all page measurements in the same capture.

Hidden launches often have no main-window handle. Input idle includes PowerShell
launch overhead and does not mean first paint or complete initial data. The script
does not substitute the ten-second observation timeout for startup latency. CPU
and memory apply to the measured application process, excluding helper processes.
These short runs do not establish cold-start speed, interactive rendering latency,
power consumption, or absence of long-term leaks. Compare the raw handle/private
growth fields for suspected leaks and arrange a longer focused observation only
when there is evidence to investigate.

## One improvement per cycle

1. Read the last comparison and select one concrete bottleneck or missing feature.
   Prefer avoiding unnecessary sampling, repeated allocation, repeated registry/SCM
   queries, and idle repainting. Record the suspected cause and measurable effect.
2. Capture a baseline in quiet, consistent conditions. For UI changes, inspect the
   affected page and preserve its fonts, spacing, keyboard behavior, and selection.
3. Implement the smallest useful change. Reuse current tests. Add only a narrow
   regression check for meaningful new behavior or a demonstrated failure.
4. Run `cargo fmt --all -- --check`, relevant existing tests, and a release build.
   Run the existing full test suite and Clippy once before a reviewable completion,
   rather than repeatedly during every small edit. Use the existing `--self-test`
   only when hardware/provider integration needs checking.
5. Repeat the same capture and compare. Investigate a regression; rerun a noisy
   sample in quieter conditions. Do not repeatedly rerun until a result happens to
   pass, weaken thresholds, or silently accept an incompatible baseline.
6. Record the change, evidence, and limits in the branch or review. Select a new
   baseline only after reviewing the result. Never push to `main`, merge, publish,
   or change release settings as part of this loop.

A recurring Codex follow-up can run this workflow in bounded cycles. It should
reuse the current work branch, avoid interrupting active edits/benchmarks, report
only useful changes or actionable failures, and preserve the user's reviewed
design. Scheduling is separate from these scripts; running a capture does not
install an operating-system task or add background work to Feather.

The current chat has an active daily follow-up, **Feather 성능·기능 지속 개선**,
which follows `IMPROVEMENTS.md` on `codex/task-manager-improvements`. Its schedule
is managed by the Codex app and is not installed by cloning this repository.
