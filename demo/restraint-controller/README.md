# restraint-controller: a Floyd MC/DC demo

A small safety-controller example. `arm_frontal_airbag` in
[`src/lib.rs`](src/lib.rs) decides whether to arm a frontal airbag:

```text
arm iff: occupant_present
     AND (crash_detected
          OR (speed_above_threshold AND NOT seatbelt_fastened))
```

As shipped, the crate's tests reach 100% MC/DC of that decision. The
walkthrough then plants a wiring bug (the `!` on `seatbelt_fastened`
is dropped) and keeps only the "shippable" suite in
[`tests/weak.rs`](tests/weak.rs). That suite still passes, and every
line of the function still runs, but Floyd shows that two of the four
conditions are never demonstrated. Writing the tests Floyd asks for
([`tests/mcdc.rs`](tests/mcdc.rs)) exposes the bug.

## Setup

Floyd needs a nightly toolchain with the LLVM tools. The walkthrough
below was recorded with `cargo-floyd` built from this repository and
nightly-2026-11-03:

```sh
rustup install nightly
rustup component add llvm-tools-preview --toolchain nightly
cargo install --path cargo-floyd     # from the repository root
cd demo/restraint-controller
```

The demo is its own workspace (it has an empty `[workspace]` table),
so its build does not touch the Floyd workspace. Paths in the outputs
below are shortened to `/path/to/floyd`.

`cargo test` and `cargo floyd test` can be run in any order, with
any toolchain: Floyd builds its instrumented test binaries in
`target/floyd`, separate from the ones `cargo test` builds in
`target/debug`. (Earlier versions shared `target/debug` and stopped
with exit 1 after a `cargo +nightly test`; that is fixed.)

## Step 0: the clean state, 100% MC/DC

```sh
cargo test          # 5 tests pass
cargo floyd test    # exit code 0
```

```
floyd  runtime MC/DC analysis of /path/to/floyd/demo/restraint-controller/Cargo.toml

Test targets: 3
Tests discovered: 5
Observations:     5 (5 used, 0 ambiguous)
Conditions:       !seatbelt_fastened, crash_detected, occupant_present, speed_above_threshold

Per-test observations:
  arms_at_high_speed_when_occupant_unbelted_and_no_crash  !seatbelt_fastened=T  crash_detected=F  occupant_present=T  speed_above_threshold=T  result=T
  no_arm_at_high_speed_when_occupant_belted_and_no_crash  !seatbelt_fastened=F  crash_detected=F  occupant_present=T  speed_above_threshold=T  result=F
  arms_when_crash_detected_with_occupant  !seatbelt_fastened=-  crash_detected=T  occupant_present=T  speed_above_threshold=-  result=T
  no_arm_at_low_speed_without_crash  !seatbelt_fastened=-  crash_detected=F  occupant_present=T  speed_above_threshold=F  result=F
  no_arm_without_occupant         !seatbelt_fastened=-  crash_detected=-  occupant_present=F  speed_above_threshold=-  result=F

Per-condition MC/DC status:
  ✓ !seatbelt_fastened: EXERCISED  via (!seatbelt_fastened=F, crash_detected=F, occupant_present=T, speed_above_threshold=T) -> F   vs   (!seatbelt_fastened=T, crash_detected=F, occupant_present=T, speed_above_threshold=T) -> T
  ✓ crash_detected: EXERCISED  via (!seatbelt_fastened=F, crash_detected=F, occupant_present=T, speed_above_threshold=F) -> F   vs   (!seatbelt_fastened=F, crash_detected=T, occupant_present=T, speed_above_threshold=F) -> T
  ✓ occupant_present: EXERCISED  via (!seatbelt_fastened=F, crash_detected=F, occupant_present=F, speed_above_threshold=F) -> F   vs   (!seatbelt_fastened=T, crash_detected=T, occupant_present=T, speed_above_threshold=T) -> T
  ✓ speed_above_threshold: EXERCISED  via (!seatbelt_fastened=T, crash_detected=F, occupant_present=T, speed_above_threshold=F) -> F   vs   (!seatbelt_fastened=T, crash_detected=F, occupant_present=T, speed_above_threshold=T) -> T

MC/DC coverage: 4 of 4 conditions exercised (100%)

Out of scope: 5 function(s) have a decision in a shape the engine's current patterns do not recognise. The engine declined them rather than report a wrong verdict. See --format=json's `out_of_scope` for the full list.
```

Every condition has an independence pair: two condition vectors that
differ only in that condition and give different outcomes. In the
pair rows a condition the test short-circuited is shown with a value
(here `F`) because the row is a truth-table row; the per-test
observations above show it as `-`. Each test evaluates the decision
exactly once, so every observation is *used*.

The "Out of scope" line is about the five `#[test]` functions
themselves (their `assert!` expansions), which Floyd declines rather
than analyse; it does not affect the verdict or the exit code.

The same run in the other formats (both exit 0):

```sh
cargo floyd test --format=json    # "verdict": "analysed", summary below
cargo floyd test --format=junit
```

```json
"summary": {
  "conditions_total": 4,
  "exercised": 4,
  "not_exercised": 0,
  "unknown": 0,
  "not_measurable": 0,
  "percentage": 100.0
}
```

```xml
<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="floyd" tests="4" failures="0">
  <testsuite name="/path/to/floyd/demo/restraint-controller/Cargo.toml" tests="4" failures="0">
    <properties>
      <property name="floyd.conditions_total" value="4"/>
      <property name="floyd.exercised" value="4"/>
      <property name="floyd.not_exercised" value="0"/>
      <property name="floyd.unknown" value="0"/>
      <property name="floyd.not_measurable" value="0"/>
      <property name="floyd.percentage" value="100"/>
    </properties>
    <testcase name="!seatbelt_fastened" classname="floyd.mcdc"/>
    <testcase name="crash_detected" classname="floyd.mcdc"/>
    <testcase name="occupant_present" classname="floyd.mcdc"/>
    <testcase name="speed_above_threshold" classname="floyd.mcdc"/>
  </testsuite>
</testsuites>
```

## Step 1: plant the bug, keep only the weak suite

```sh
perl -pi -e 's/&& !seatbelt_fastened/&& seatbelt_fastened/' src/lib.rs
mv tests/mcdc.rs mcdc.rs.off
cargo test          # weak.rs: 3 passed
cargo floyd test    # exit code 2
```

The buggy line is now

```rust
    occupant_present
        && (crash_detected || (speed_above_threshold && seatbelt_fastened))
```

and `cargo test` is green. Floyd:

```
floyd  runtime MC/DC analysis of /path/to/floyd/demo/restraint-controller/Cargo.toml

Test targets: 2
Tests discovered: 3
Observations:     3 (3 used, 0 ambiguous)
Conditions:       crash_detected, occupant_present, seatbelt_fastened, speed_above_threshold

Per-test observations:
  arms_when_crash_detected_with_occupant  crash_detected=T  occupant_present=T  seatbelt_fastened=-  speed_above_threshold=-  result=T
  no_arm_at_low_speed_without_crash  crash_detected=F  occupant_present=T  seatbelt_fastened=-  speed_above_threshold=F  result=F
  no_arm_without_occupant         crash_detected=-  occupant_present=F  seatbelt_fastened=-  speed_above_threshold=-  result=F

Per-condition MC/DC status:
  ✓ crash_detected: EXERCISED  via (crash_detected=F, occupant_present=T, seatbelt_fastened=F, speed_above_threshold=F) -> F   vs   (crash_detected=T, occupant_present=T, seatbelt_fastened=F, speed_above_threshold=F) -> T
  ✓ occupant_present: EXERCISED  via (crash_detected=F, occupant_present=F, seatbelt_fastened=F, speed_above_threshold=F) -> F   vs   (crash_detected=T, occupant_present=T, seatbelt_fastened=T, speed_above_threshold=T) -> T
  ✗ seatbelt_fastened: UNEXERCISED — needs tests for both (crash_detected=F, occupant_present=T, seatbelt_fastened=F, speed_above_threshold=T) -> F and (crash_detected=F, occupant_present=T, seatbelt_fastened=T, speed_above_threshold=T) -> T
  ✗ speed_above_threshold: UNEXERCISED — needs a test where (crash_detected=F, occupant_present=T, seatbelt_fastened=T, speed_above_threshold=T) -> T [the matching row (crash_detected=F, occupant_present=T, seatbelt_fastened=T, speed_above_threshold=F) -> F is already covered]

MC/DC coverage: 2 of 4 conditions exercised (50%)

Out of scope: 3 function(s) have a decision in a shape the engine's current patterns do not recognise. The engine declined them rather than report a wrong verdict. See --format=json's `out_of_scope` for the full list.
```

Exit code 2 means a decision was analysed but MC/DC was not
demonstrated. The two findings are provably UNEXERCISED, not UNKNOWN:
every test evaluated the decision once, so there are no ambiguous
observations that could hold the missing rows. `seatbelt_fastened`
was never evaluated (every test short-circuits before it), and
`speed_above_threshold` was only ever false.

For each unexercised condition Floyd names an independence pair from
the truth table of the code as written: two rows that differ only in
that condition, with the outcome each gives. It then says which of
the two rows still need a test. A row counts as covered when an
existing test matches it on every condition the test evaluated: for
`speed_above_threshold`, `no_arm_at_low_speed_without_crash`
short-circuits `seatbelt_fastened` and so already covers the
`speed_above_threshold=F` row, leaving one row to test. Neither
`seatbelt_fastened` row is covered, so both need a test.

Read the pair for `speed_above_threshold` against the spec: Floyd
says the speed condition only matters when `seatbelt_fastened=T`.
The spec says it should matter when the occupant is *not* belted.
That mismatch is the bug, and it is visible before any new test is
written.

`--format=json` (exit 2) reports
`"seatbelt_fastened": "Unexercised"` and
`"speed_above_threshold": "Unexercised"` in
`analysis.condition_status`, the rows still needed in
`analysis.missing_rows` (two for `seatbelt_fastened`, one for
`speed_above_threshold`), and

```json
"summary": {
  "conditions_total": 4,
  "exercised": 2,
  "not_exercised": 2,
  "unknown": 0,
  "not_measurable": 0,
  "percentage": 50.0
}
```

`--format=junit` (exit 2) fails one testcase per unexercised
condition, with the same message as the text report:

```xml
    <testcase name="seatbelt_fastened" classname="floyd.mcdc">
      <failure message="needs tests for both (crash_detected=F, occupant_present=T, seatbelt_fastened=F, speed_above_threshold=T) -&gt; F and (crash_detected=F, occupant_present=T, seatbelt_fastened=T, speed_above_threshold=T) -&gt; T" type="UnexercisedCondition"/>
    </testcase>
    <testcase name="speed_above_threshold" classname="floyd.mcdc">
      <failure message="needs a test where (crash_detected=F, occupant_present=T, seatbelt_fastened=T, speed_above_threshold=T) -&gt; T [the matching row (crash_detected=F, occupant_present=T, seatbelt_fastened=T, speed_above_threshold=F) -&gt; F is already covered]" type="UnexercisedCondition"/>
    </testcase>
```

## Step 2: write the tests Floyd asks for

[`tests/mcdc.rs`](tests/mcdc.rs) adds the two rows with
`occupant_present=T, crash_detected=F, speed_above_threshold=T`, one
unbelted and one belted, with the outcomes the spec requires:

```sh
mv mcdc.rs.off tests/mcdc.rs
cargo test          # mcdc.rs: 2 failed
cargo floyd test    # exit code 0
```

Both new tests fail against the buggy code: that is the bug caught.

```
test arms_at_high_speed_when_occupant_unbelted_and_no_crash ... FAILED
test no_arm_at_high_speed_when_occupant_belted_and_no_crash ... FAILED
thread 'arms_at_high_speed_when_occupant_unbelted_and_no_crash' (<id>) panicked at tests/mcdc.rs:31:5:
thread 'no_arm_at_high_speed_when_occupant_belted_and_no_crash' (<id>) panicked at tests/mcdc.rs:39:5:
test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

Floyd now reports `MC/DC coverage: 4 of 4 conditions exercised
(100%)` and exit 0 for the buggy code: MC/DC measures whether the
tests demonstrate each condition's effect on the outcome, not
whether the outcome is right, and Floyd analyses the failing tests'
evaluations like any others. Its job was to point at the tests that
were missing; the assertions in those tests are what catch the bug.
Gate CI on both `cargo test` and `cargo floyd test`.

## Step 3: fix the bug

```sh
git checkout -- src/lib.rs
cargo test          # 5 passed
cargo floyd test    # exit code 0
```

The output is identical to step 0 (100%, exit 0 in all three
formats).

## Checking Floyd against llvm-cov

Floyd's per-condition counts can be checked with
`llvm-cov show --show-branches=count` on the merge of the profiles of
a run. For the clean state (step 0), the decision's branch counters
are

```
   52|      4|        && (crash_detected || (speed_above_threshold && !seatbelt_fastened))
  |  Branch (52:13): [True: 1, False: 3]
  |  Branch (52:32): [True: 2, False: 1]
  |  Branch (52:57): [True: 1, False: 1]
```

with `occupant_present` (51:5) at `[True: 4, False: 1]`, which is
exactly the per-test observations summed. For step 1 they are
`occupant_present` `[True: 2, False: 1]`, `crash_detected`
`[True: 1, False: 1]`, `speed_above_threshold` `[True: 0, False: 1]`
and `seatbelt_fastened` `[True: 0, False: 0]`: the two conditions
Floyd reports as UNEXERCISED are the two with a zero counter.

## Variant: one table-driven test

Floyd's rule is one condition vector per evaluation it can identify.
A test that evaluates the decision more than once is still resolved
when the decision is a *tree* (every condition is reached along one
path only), because the branch counters then give each outcome leaf's
count (see [Tests that evaluate a decision more than
once](../../README.md#tests-that-evaluate-a-decision-more-than-once)).
This decision is a tree, so replacing both test files with a single
loop over the five vectors still gives 100%:

```rust
use restraint_controller::arm_frontal_airbag;

#[test]
fn spec_table() {
    // (occupant, crash, speed, belted) -> armed
    let table = [
        ((true, true, false, true), true),
        ((false, true, true, false), false),
        ((true, false, false, true), false),
        ((true, false, true, false), true),
        ((true, false, true, true), false),
    ];
    for ((o, c, s, b), want) in table {
        assert_eq!(arm_frontal_airbag(o, c, s, b), want);
    }
}
```

```
Test targets: 2
Tests discovered: 1
Observations:     5 (5 used, 0 ambiguous)
Conditions:       !seatbelt_fastened, crash_detected, occupant_present, speed_above_threshold

Per-test observations:
  spec_table                      !seatbelt_fastened=-  crash_detected=T  occupant_present=T  speed_above_threshold=-  result=T
  spec_table                      !seatbelt_fastened=T  crash_detected=F  occupant_present=T  speed_above_threshold=T  result=T
  spec_table                      !seatbelt_fastened=F  crash_detected=F  occupant_present=T  speed_above_threshold=T  result=F
  spec_table                      !seatbelt_fastened=-  crash_detected=F  occupant_present=T  speed_above_threshold=F  result=F
  spec_table                      !seatbelt_fastened=-  crash_detected=-  occupant_present=F  speed_above_threshold=-  result=F

Per-condition MC/DC status:
  ✓ !seatbelt_fastened: EXERCISED  via (!seatbelt_fastened=F, crash_detected=F, occupant_present=T, speed_above_threshold=T) -> F   vs   (!seatbelt_fastened=T, crash_detected=F, occupant_present=T, speed_above_threshold=T) -> T
  ✓ crash_detected: EXERCISED  via (!seatbelt_fastened=F, crash_detected=F, occupant_present=T, speed_above_threshold=F) -> F   vs   (!seatbelt_fastened=F, crash_detected=T, occupant_present=T, speed_above_threshold=F) -> T
  ✓ occupant_present: EXERCISED  via (!seatbelt_fastened=F, crash_detected=F, occupant_present=F, speed_above_threshold=F) -> F   vs   (!seatbelt_fastened=T, crash_detected=T, occupant_present=T, speed_above_threshold=T) -> T
  ✓ speed_above_threshold: EXERCISED  via (!seatbelt_fastened=T, crash_detected=F, occupant_present=T, speed_above_threshold=F) -> F   vs   (!seatbelt_fastened=T, crash_detected=F, occupant_present=T, speed_above_threshold=T) -> T

MC/DC coverage: 4 of 4 conditions exercised (100%)
```

Exit code 0. In `--format=json` each of the five `spec_table` observations carries
`"resolved_by": "tree_counters"`.
