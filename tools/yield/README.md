# Yield table tooling

[`YIELD.md`](../../YIELD.md) at the repo root is generated from a sweep: Floyd run on a fixed
set of open-source crates at pinned commits. Nothing in it is edited by hand.

| file | what it is |
|---|---|
| `crates.tsv` | the pinned crate list: name, upstream, full commit, package directory, `cargo floyd` flags |
| `sweep.sh` | runs the sweep (builds Floyd from this checkout, clones each crate, runs it) |
| `render_yield.py` | turns a sweep directory into `YIELD.md` (Python 3 standard library only) |

## Regenerate YIELD.md from an existing sweep

```sh
python3 tools/yield/render_yield.py SWEEP_DIR            # writes ./YIELD.md
python3 tools/yield/render_yield.py SWEEP_DIR -o -       # print instead
```

The output depends only on `SWEEP_DIR` (no current date or environment), so running it twice
gives the same file. The definitions behind each column are in the script's docstring and in
the "How to read this" section of `YIELD.md`.

## Re-run the sweep

```sh
export PATH=$HOME/.cargo/bin:$PATH CARGO_BUILD_JOBS=3
tools/yield/sweep.sh ~/floyd-sweep-N          # all crates; takes hours
tools/yield/sweep.sh ~/floyd-sweep-N hex url  # a subset
python3 tools/yield/render_yield.py ~/floyd-sweep-N
```

`sweep.sh` builds `cargo-floyd` and `floyd-rustc-shim` in release mode from this checkout and
copies them to `SWEEP_DIR/bin/` (with the commit in `bin/git_rev`), records `TOOLCHAIN` and
`MACHINE`, then for each crate in `crates.tsv` clones the upstream into `SWEEP_DIR/<crate>/src`,
checks out the pinned commit, and runs

```sh
cargo floyd test --format=json --all-candidates <flags> .
```

in the package directory, writing `SWEEP_DIR/<crate>/out/{span.json,stderr.log,exit_code,flags,git_rev}`
and timestamps to `SWEEP_DIR/sweep.log`. If `timeout` (or `gtimeout`) is on `PATH`, each crate
is limited to `LIMIT_SECS` seconds (default 3600).

Flags: `time` runs with `--all-features --max-mir-size=150`, `fixed` with `--max-mir-size=200`
(their largest MIR bodies exceed the default size limit), every other crate with none.
`heapless` currently fails to build under the instrumented nightly (a rustc ICE) and appears in
the table as `build_failed`.

## Differences from the sweep behind the current YIELD.md

The current `YIELD.md` was rendered from sweep 11, the 0.4.0 release gate, which ran with an
internal runner rather than `sweep.sh`. `sweep.sh` is a cleaned-down version of it with the same
crate list, commits, flags and JSON command. It has been syntax-checked but not yet run
end to end; the first real run should be compared against sweep 11. What the internal runner also did, and `sweep.sh`
does not:

- ran the `text` and `junit` formats as well (not needed for the table);
- checked after every format that the crate's working tree was left clean (`git status
  --ignored` before and after, and no `*.profraw` outside `target/`);
- copied crates from local clones instead of cloning, and kept MIR dumps for crates without an
  analysed decision;
- cross-checked every analysed decision against an independent re-derivation from the raw
  coverage counters (no disagreement in sweep 11).

The internal runner also wrote a `HEADER.md` instead of `MACHINE`; `render_yield.py` reads the
machine description from either.

`cortex-m` ships no `Cargo.lock`, so its dependencies resolve at sweep time and can drift between
sweeps (sweep 11 saw `syn` 3.0.10, the sweep before 3.0.9). Eleven other crates also ship no lock
file; for them too a re-run is pinned at the crate, not at its dependencies.

## Future work

- Regenerate the sweep and `YIELD.md` in CI (scheduled, not per-PR, given the run time). Not set
  up today; the table is refreshed by hand at release gates.
- Carry the clean-tree check and the independent cross-check into `sweep.sh`.
