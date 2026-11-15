#!/usr/bin/env python3
"""Render YIELD.md (Floyd's per-crate yield table) from a sweep directory.

Usage:
    python3 tools/yield/render_yield.py SWEEP_DIR [-o OUTPUT]

SWEEP_DIR is the output of a sweep (see tools/yield/README.md):

    SWEEP_DIR/TOOLCHAIN                 rustc version string (first line)
    SWEEP_DIR/bin/git_rev               Floyd commit the sweep's binary was built from
    SWEEP_DIR/sweep.log                 START/SWEEP_DONE lines with ISO timestamps (dates)
    SWEEP_DIR/MACHINE or HEADER.md      machine description (optional)
    SWEEP_DIR/<crate>/out/span.json     `cargo floyd test --format=json --all-candidates <flags>`
    SWEEP_DIR/<crate>/out/flags         the <flags> used
    SWEEP_DIR/<crate>/out/git_rev       the crate's pinned commit
    SWEEP_DIR/<crate>/out/exit_code     exit code of the json run (optional)

Definitions used in the table (also printed in YIELD.md):

  functions with a decision
      = len(candidates) + the number of `out_of_scope` entries whose reason
        is NOT `no_decision`. Candidates are decisions Floyd recovered; the
        other out_of_scope reasons (`unsupported_shape`,
        `too_many_conditions`, `non_bool_leaf`) are functions that do
        contain a decision Floyd declined. `no_decision` entries contain no
        user-written Boolean decision and are left out of the denominator.
        Entries are counted as Floyd reports them (one per MIR body), so
        monomorphised or macro-stamped copies of one source function count
        separately.
  analysed
      = len(analyses): recovered decisions with an MC/DC analysis.
  conditions by status
      = condition_status of every entry in `analyses` (all candidates, not
        only the selected decision that `summary` describes).

Output is a pure function of SWEEP_DIR: no current date, no environment.
Python 3 standard library only.
"""

import argparse
import collections
import json
import os
import re
import sys

# Status order for the condition columns; any status not listed here is
# appended after these, so new engine statuses are never dropped silently.
STATUS_ORDER = ["Exercised", "ExercisedUnidentified", "Unexercised", "Ambiguous"]
STATUS_LABEL = {
    "Exercised": "Exercised",
    "ExercisedUnidentified": "ExercisedUnidentified",
    "Unexercised": "Unexercised",
    "Ambiguous": "UNKNOWN (multi-evaluation)",
}
DECLINE_ORDER = ["unsupported_shape", "too_many_conditions", "non_bool_leaf"]
NO_DECISION = "no_decision"


def read_text(path, default=None):
    try:
        with open(path, encoding="utf-8") as f:
            return f.read().strip()
    except OSError:
        return default


def ordered(keys, preferred):
    keys = set(keys)
    return [k for k in preferred if k in keys] + sorted(keys - set(preferred))


def fmt_breakdown(counter):
    if not counter or sum(counter.values()) == 0:
        return "0"
    parts = ", ".join(f"{k} {v}" for k, v in sorted(counter.items()) if v)
    return f"{sum(counter.values())} ({parts})"


def load_crate(sweep, name):
    out = os.path.join(sweep, name, "out")
    with open(os.path.join(out, "span.json"), encoding="utf-8") as f:
        d = json.load(f)
    cands = d.get("candidates") or []
    analyses = d.get("analyses") or []
    oos = d.get("out_of_scope") or []
    oos_summary = d.get("out_of_scope_summary") or {}

    analysed_idx = {a.get("candidate") for a in analyses}
    nm = collections.Counter()
    residual = 0
    for i, c in enumerate(cands):
        if c.get("not_measurable"):
            nm[c["not_measurable"]["reason"]] += 1
        elif i not in analysed_idx:
            residual += 1

    status = collections.Counter()
    for a in analyses:
        for st in (a.get("condition_status") or {}).values():
            status[st if isinstance(st, str) else next(iter(st))] += 1

    oos_reason = collections.Counter(o.get("reason") for o in oos)
    declined = collections.Counter(
        {k: v for k, v in oos_reason.items() if k != NO_DECISION}
    )
    no_dec_detail = collections.Counter(
        o.get("detail") or "(none)" for o in oos if o.get("reason") == NO_DECISION
    )
    flags = read_text(os.path.join(out, "flags"), "")
    return {
        "name": name,
        "rev": read_text(os.path.join(out, "git_rev"), "?"),
        "flags": flags,
        "exit": read_text(os.path.join(out, "exit_code")),
        "verdict": d.get("verdict"),
        "error": d.get("error"),
        "toolchain": (d.get("toolchain") or {}).get("version"),
        "host": (d.get("toolchain") or {}).get("host"),
        "candidates": len(cands),
        "analysed": len(analyses),
        "nm": nm,
        "residual": residual,
        "status": status,
        "declined": declined,
        "no_decision": oos_reason.get(NO_DECISION, 0),
        "no_dec_detail": no_dec_detail,
        "ctfe": oos_summary.get("ctfe_bodies_skipped", 0) or 0,
        "with_decision": len(cands) + sum(declined.values()),
        "oos_keys": collections.Counter(
            (o.get("name"), o.get("source_file"), o.get("line"), o.get("reason"))
            for o in oos
        ),
    }


def sweep_dates(sweep):
    log = read_text(os.path.join(sweep, "sweep.log"), "")
    stamps = re.findall(r"^(\d{4}-\d{2}-\d{2})T\S+ (?:START|SWEEP_DONE)", log, re.M)
    if not stamps:
        return "unknown"
    first, last = min(stamps), max(stamps)
    return first if first == last else f"{first} to {last}"


def machine(sweep, hosts):
    m = read_text(os.path.join(sweep, "MACHINE"))
    if m:
        return m.splitlines()[0]
    header = read_text(os.path.join(sweep, "HEADER.md"), "")
    found = re.search(r"(macOS [0-9.]+ \S+?|Linux \S+ \S+?)[,.]? ", header)
    jobs = re.search(r"CARGO_BUILD_JOBS=(\d+)", header)
    parts = []
    if found:
        parts.append(found.group(1))
    parts.append("host " + ", ".join(sorted(hosts)) if hosts else "host unknown")
    if jobs:
        parts.append(f"CARGO_BUILD_JOBS={jobs.group(1)}")
    return ", ".join(parts)


def display_path(path):
    path = os.path.abspath(path)
    home = os.path.expanduser("~")
    if path == home or path.startswith(home + os.sep):
        return "~" + path[len(home):]
    return path


def render(sweep):
    names = sorted(
        n for n in os.listdir(sweep)
        if os.path.isfile(os.path.join(sweep, n, "out", "span.json"))
    )
    if not names:
        sys.exit(f"no <crate>/out/span.json under {sweep}")
    crates = [load_crate(sweep, n) for n in names]
    ok = [c for c in crates if c["verdict"] != "build_failed"]
    failed = [c for c in crates if c["verdict"] == "build_failed"]

    floyd_rev = read_text(os.path.join(sweep, "bin", "git_rev"), "unknown")
    tc_file = (read_text(os.path.join(sweep, "TOOLCHAIN"), "") or "").splitlines()
    tc_json = sorted({c["toolchain"] for c in crates if c["toolchain"]})
    toolchain = tc_file[0] if tc_file else (tc_json[0] if tc_json else "unknown")
    tc_note = ""
    if tc_json and tc_json != [toolchain]:
        tc_note = f" (JSON `toolchain.version` reports: {', '.join(tc_json)})"
    hosts = {c["host"] for c in crates if c["host"]}

    decl_cols = ordered({k for c in crates for k in c["declined"]}, DECLINE_ORDER)
    for k in DECLINE_ORDER[:2]:  # always show the two named decline reasons
        if k not in decl_cols:
            decl_cols.insert(DECLINE_ORDER.index(k), k)
    stat_cols = ordered({k for c in crates for k in c["status"]} | set(STATUS_ORDER), STATUS_ORDER)
    show_residual = any(c["residual"] for c in crates)

    L = []
    w = L.append
    w("# Floyd yield table")
    w("")
    w("<!-- Generated by tools/yield/render_yield.py. Do not edit by hand. -->")
    w("")
    w("What Floyd recovers, declines and analyses on real crates, one row per crate,")
    w("from a sweep of pinned open-source crates.")
    w("")
    w(f"- **Floyd commit:** `{floyd_rev[:12]}` (the `cargo-floyd` binary the sweep ran)")
    w(f"- **Toolchain:** `{toolchain}`{tc_note}")
    w(f"- **Sweep date:** {sweep_dates(sweep)}")
    w(f"- **Machine:** {machine(sweep, hosts)}")
    w(f"- **Crates:** {len(crates)} ({len(failed)} did not build: "
      + (", ".join(f"{c['name']}" for c in failed) or "none") + ")")
    w("- **Per crate:** `cargo floyd test --format=json --all-candidates <flags> .`"
      " at the pinned commit (crate list: [`tools/yield/crates.tsv`](tools/yield/crates.tsv))")
    w("- **Regenerate this file:** `python3 tools/yield/render_yield.py "
      f"{display_path(sweep)}` from the repo root (the sweep directory is the output of"
      " [`tools/yield/sweep.sh`](tools/yield/sweep.sh); it is not checked in)")
    w("")
    w("## How to read this")
    w("")
    w("*Functions with a decision* is the denominator: decisions Floyd recovered"
      " (*recovered*, the JSON `candidates`) plus `out_of_scope` entries whose reason is"
      " not `no_decision`, i.e. functions that contain a decision Floyd declined."
      " Entries are counted as Floyd reports them, one per MIR body, so monomorphised or"
      " macro-stamped copies of one source function count separately."
      " `no_decision` entries (assert-only, derived impls, plain enum matches, `?`/loop"
      " plumbing, drop flags) contain no user-written Boolean decision and are listed"
      " separately below. A recovered decision is either *analysed* (MC/DC analysis"
      " against the tests' coverage) or *not measurable* (its coverage cannot be"
      " attributed; reasons as in the JSON). Condition columns count every condition of"
      " every analysed decision, not only the decision the text report selects."
      " *UNKNOWN (multi-evaluation)* is the `Ambiguous` status: a test evaluated the"
      " decision more than once and the aggregate counters cannot say which rows ran.")
    w("")
    w("## What this table does not claim")
    w("")
    w(f"- It is a sample of {len(crates)} crates chosen for variety, not a survey of"
      " crates.io. The numbers say nothing about crates outside the sample.")
    w("- The analysed share is small. Most functions with a decision are declined, and"
      " the declines are reported as declines: Floyd returns no decision rather than a"
      " guessed one, so a low yield is a scope limit, not wrong output.")
    w("- An Exercised count is not a coverage score for the crate. It covers only the"
      " analysed decisions, with the crate's own test suite.")
    w("")
    w("## Per crate")
    w("")

    head = (["crate", "commit", "flags", "verdict (exit)", "functions with a decision",
             "recovered"]
            + [f"declined: {k}" for k in decl_cols]
            + (["recovered, neither analysed nor not measurable"] if show_residual else [])
            + ["not measurable (by reason)", "analysed"]
            + [f"conditions: {STATUS_LABEL.get(k, k)}" for k in stat_cols])
    w("| " + " | ".join(head) + " |")
    w("|" + "|".join(["---"] * 4 + ["---:"] * (len(head) - 4)) + "|")

    def row(cells):
        w("| " + " | ".join(str(x) for x in cells) + " |")

    for c in crates:
        flags = f"`{c['flags']}`" if c["flags"] else "-"
        verdict = f"{c['verdict']} ({c['exit']})" if c["exit"] is not None else c["verdict"]
        if c["verdict"] == "build_failed":
            n = len(head) - 4
            row([c["name"], f"`{c['rev']}`", flags, verdict] + ["-"] * n)
            continue
        row([c["name"], f"`{c['rev']}`", flags, verdict, c["with_decision"], c["candidates"]]
            + [c["declined"].get(k, 0) for k in decl_cols]
            + ([c["residual"]] if show_residual else [])
            + [fmt_breakdown(c["nm"]), c["analysed"]]
            + [c["status"].get(k, 0) for k in stat_cols])

    def total(key):
        return sum(c[key] for c in ok)

    def total_counter(key):
        t = collections.Counter()
        for c in ok:
            t.update(c[key])
        return t

    tdecl, tnm, tstat = total_counter("declined"), total_counter("nm"), total_counter("status")
    row([f"**total ({len(ok)} crates with output)**", "", "", "",
         f"**{total('with_decision')}**", f"**{total('candidates')}**"]
        + [f"**{tdecl.get(k, 0)}**" for k in decl_cols]
        + ([f"**{total('residual')}**"] if show_residual else [])
        + [f"**{fmt_breakdown(tnm)}**", f"**{total('analysed')}**"]
        + [f"**{tstat.get(k, 0)}**" for k in stat_cols])
    w("")

    twd, tan = total("with_decision"), total("analysed")
    share = f"{100.0 * tan / twd:.1f}%" if twd else "n/a"
    w(f"Analysed decisions over functions with a decision: {tan} / {twd} = {share}.")
    if not show_residual:
        w("Every recovered decision is either analysed or not measurable.")
    w("")

    # Overlap between rows that ran on the same repository at the same commit
    # (a workspace run can include a member that also has its own row).
    notes = []
    for i, a in enumerate(ok):
        for b in ok[i + 1:]:
            if a["rev"] != b["rev"]:
                continue
            both = a["oos_keys"] & b["oos_keys"]
            common = sum(both.values())
            if common:
                declined = sum(v for k, v in both.items() if k[3] != NO_DECISION)
                notes.append(
                    f"- `{a['name']}` and `{b['name']}` are the same repository at commit"
                    f" `{a['rev']}`; {common} `out_of_scope` entries (same name, file, line"
                    f" and reason) appear in both rows, {declined} of them declined (in the"
                    " denominator) and the rest `no_decision`. The totals count them twice.")
    for c in failed:
        err = c["error"] or {}
        msg = err.get("message") if isinstance(err, dict) else str(err)
        notes.append(f"- `{c['name']}`: build failed, no analysis (Floyd's error: \"{msg}\")."
                     " Excluded from the totals.")
    if notes:
        w("## Notes")
        w("")
        L.extend(notes)
        w("")

    w("## Not in the denominator")
    w("")
    w("`no_decision` entries by detail, and CTFE bodies (compile-time evaluation copies of"
      " runtime functions, `out_of_scope_summary.ctfe_bodies_skipped`), which Floyd counts"
      " but does not list.")
    w("")
    details = ordered({k for c in ok for k in c["no_dec_detail"]}, [])
    h2 = ["crate", "no_decision"] + [f"no_decision: {k}" for k in details] + ["CTFE bodies skipped"]
    w("| " + " | ".join(h2) + " |")
    w("|---|" + "|".join(["---:"] * (len(h2) - 1)) + "|")
    for c in ok:
        row([c["name"], c["no_decision"]] + [c["no_dec_detail"].get(k, 0) for k in details]
            + [c["ctfe"]])
    tdet = total_counter("no_dec_detail")
    row(["**total**", f"**{total('no_decision')}**"]
        + [f"**{tdet.get(k, 0)}**" for k in details] + [f"**{total('ctfe')}**"])
    w("")
    return "\n".join(L).rstrip("\n") + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("sweep", help="sweep directory (contains <crate>/out/span.json)")
    ap.add_argument("-o", "--output", default="YIELD.md", help="output file (default YIELD.md; '-' for stdout)")
    args = ap.parse_args()
    text = render(args.sweep)
    if args.output == "-":
        sys.stdout.write(text)
    else:
        with open(args.output, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)


if __name__ == "__main__":
    main()
