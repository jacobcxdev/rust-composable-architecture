# Benchmark RCA

If you are building an application, start with [measure your app](../guides/performance.md). The benchmarks here measure RCA itself.

| What do you want to know? | Start here |
| --- | --- |
| Did my change affect a particular API? | [Run one workload](../../tests/performance-profile/README.md#run-one-workload). It checks that the operation did the expected work. |
| What does using RCA cost at build time? | The [four small applications](../../benches/compile/README.md) cover a direct reducer, hand-written and macro-based composition, and a scoped Store. |
| Which checks does a pull request need? | [Choose checks for the change](../maintainers/qualification.md#start-with-the-changed-behaviour). A portable benchmark does not need a release runner. |
| Does a release meet the performance limits? | Follow the [release checks](../maintainers/qualification.md#native-review-and-release), including the [runtime](method.md) and [composition](composition/README.md) procedures. |

For a local comparison, run both revisions on the same machine with the same input, features, compiler, and build settings. Check the result before comparing the cost. Alternate which revision runs first. If the difference disappears or reverses across runs, report that uncertainty. Do not scale a measurement from one machine to predict another.

## Release runner profiles

The public benchmarks define the work, comparison rules, and allocation limits. Release checks also pin each machine. The macOS checks use absolute limits measured for that machine. The Windows checks set timing and size limits from the reference in the same run. You do not need either machine profile to try a workload.

For the macOS runtime check or either platform's composition check, set `RCA_RUNNER_PROFILE_DIR` to the absolute path of the operator's profile directory **outside the checkout**. Capture and replay require the approved `macos.json` or `windows.json`. The controller checks the file's SHA-256 digest. A missing or changed profile cannot pass.

The Windows runtime check records the machine it ran on. For release, its machine details must match the approved composition run and the [support measurements](../maintainers/qualification.md#native-review-and-release). A runtime result alone does not qualify Windows.

Keep profiles, measurements, and traces outside Git. A local result can inform a pull request, but it cannot establish a speedup for all applications.
