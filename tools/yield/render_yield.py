#!/usr/bin/env python3
"""Render YIELD.md (Floyd's per-crate yield table) from a sweep directory.

Usage (from the repo root):
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

Counting unit: a source function. Floyd's JSON has one entry per MIR body,
so a function compiled into both the library and a test harness, or
instantiated several times, appears more than once. Every entry
(`candidates`, `analyses`, `out_of_scope`, not-measurable candidates) is
deduplicated within a crate by the key (source_file, line, function name);
entries without a source file are keyed by name alone. Each key gets one
class, by precedence:

  recovered (some entry is a candidate)
      analysed        some candidate with this key has an entry in `analyses`
      not measurable  otherwise (its `not_measurable.reason`)
  declined (some out_of_scope entry has a reason other than `no_decision`;
      if several, the first of too_many_conditions, non_bool_leaf,
      unsupported_shape, then any other reason alphabetically)
  no decision (every entry is `no_decision`)

  functions with a decision = recovered + declined
  conditions by status      = condition_status of the analysed functions
                              (all of them, not only the decision the
                              text report selects)

Raw MIR-body counts are kept in a separate, labelled table.

A row whose keys (same commit) are all contained in another row's keys is
marked "included in <other>" and left out of the totals.

The header compares the sweep binary's commit with the engine sources of the
checkout this script lives in (`git diff`, read-only). Otherwise the output is
a pure function of SWEEP_DIR: no current date, no environment.
Python 3 standard library only.
"""

import argparse
import collections
import json
import os
import re
import subprocess
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
DECLINE_PRECEDENCE = ["too_many_conditions", "non_bool_leaf", "unsupported_shape"]
NO_DECISION = "no_decision"
REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ENGINE_PATHS = ["floyd", "cargo-floyd", "Cargo.toml", "Cargo.lock"]


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


def key_of(entry, name_field):
    f = entry.get("source_file")
    if f is None:
        return (None, None, entry.get(name_field))
    return (f, entry.get("line"), entry.get(name_field))


def pick_decline(reasons):
    for r in DECLINE_PRECEDENCE:
        if r in reasons:
            return r
    return sorted(reasons)[0]


def load_crate(sweep, name):
    out = os.path.join(sweep, name, "out")
    with open(os.path.join(out, "span.json"), encoding="utf-8") as f:
        d = json.load(f)
    cands = d.get("candidates") or []
    analyses = d.get("analyses") or []
    oos = d.get("out_of_scope") or []
    oos_summary = d.get("out_of_scope_summary") or {}

    cand_keys = [key_of(c, "function_name") for c in cands]
    analysed_by_key = collections.defaultdict(list)
    for a in analyses:
        i = a.get("candidate")
        if isinstance(i, int) and 0 <= i < len(cand_keys):
            k = cand_keys[i]
        else:
            k = key_of(a.get("provenance") or {}, "function_name")
        analysed_by_key[k].append(a.get("condition_status") or {})
    nm_by_key = collections.defaultdict(set)
    for c, k in zip(cands, cand_keys):
        if c.get("not_measurable"):
            nm_by_key[k].add(c["not_measurable"]["reason"])
    oos_by_key = collections.defaultdict(set)
    detail_by_key = collections.defaultdict(set)
    for o in oos:
        k = key_of(o, "name")
        oos_by_key[k].add(o.get("reason"))
        if o.get("reason") == NO_DECISION:
            detail_by_key[k].add(o.get("detail") or "(none)")

    recovered = set(cand_keys)
    analysed, nm, residual = set(), collections.Counter(), 0
    status = collections.Counter()
    status_conflicts = 0
    for k in sorted(recovered, key=repr):
        if k in analysed_by_key:
            analysed.add(k)
            copies = analysed_by_key[k]
            first = copies[0]
            if any(json.dumps(c, sort_keys=True) != json.dumps(first, sort_keys=True)
                   for c in copies[1:]):
                status_conflicts += 1
            for st in first.values():
                status[st if isinstance(st, str) else next(iter(st))] += 1
        elif nm_by_key.get(k):
            nm[sorted(nm_by_key[k])[0]] += 1
        else:
            residual += 1
    declined = collections.Counter()
    no_dec_detail = collections.Counter()
    mixed_reason = 0
    no_decision = 0
    for k, reasons in oos_by_key.items():
        if k in recovered:
            continue
        real = reasons - {NO_DECISION}
        if real:
            declined[pick_decline(real)] += 1
            if len(reasons) > 1:
                mixed_reason += 1
        else:
            no_decision += 1
            no_dec_detail[sorted(detail_by_key[k])[0]] += 1

    raw_oos = collections.Counter(o.get("reason") for o in oos)
    return {
        "name": name,
        "rev": read_text(os.path.join(out, "git_rev"), "?"),
        "flags": read_text(os.path.join(out, "flags"), ""),
        "exit": read_text(os.path.join(out, "exit_code")),
        "verdict": d.get("verdict"),
        "error": d.get("error"),
        "toolchain": (d.get("toolchain") or {}).get("version"),
        "host": (d.get("toolchain") or {}).get("host"),
        "recovered": len(recovered),
        "analysed": len(analysed),
        "nm": nm,
        "residual": residual,
        "status": status,
        "status_conflicts": status_conflicts,
        "declined": declined,
        "mixed_reason": mixed_reason,
        "no_decision": no_decision,
        "no_dec_detail": no_dec_detail,
        "ctfe": oos_summary.get("ctfe_bodies_skipped", 0) or 0,
        "with_decision": len(recovered) + sum(declined.values()),
        "keys": recovered | set(oos_by_key),
        # raw MIR-body counts
        "raw_candidates": len(cands),
        "raw_analyses": len(analyses),
        "raw_nm": sum(1 for c in cands if c.get("not_measurable")),
        "raw_declined": sum(v for r, v in raw_oos.items() if r != NO_DECISION),
        "raw_no_decision": raw_oos.get(NO_DECISION, 0),
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


def git(*args):
    try:
        r = subprocess.run(["git", "-C", REPO] + list(args), capture_output=True,
                           text=True, check=True)
        return r.stdout
    except (OSError, subprocess.CalledProcessError):
        return None


def cargo_version():
    toml = read_text(os.path.join(REPO, "cargo-floyd", "Cargo.toml"), "")
    m = re.search(r'^version\s*=\s*"([^"]+)"', toml, re.M)
    return m.group(1) if m else None


def engine_equivalence(sweep_rev):
    """Sentence on how the sweep binary's commit relates to this checkout's engine."""
    ver = cargo_version()
    this = f"this checkout (Floyd {ver})" if ver else "this checkout"
    same = f"Floyd {ver} as checked in here" if ver else "this checkout"
    if sweep_rev == "unknown":
        return "The sweep did not record its Floyd commit."
    diff = git("diff", "-U0", sweep_rev, "--", *ENGINE_PATHS)
    if diff is None:
        return f"Not compared with {this}: git or commit `{sweep_rev[:12]}` unavailable."
    files, other = [], []
    path = None
    for line in diff.splitlines():
        if line.startswith("+++ ") or line.startswith("--- "):
            if line.startswith("+++ "):
                path = line[6:] if line.startswith("+++ b/") else line[4:]
                if path not in files:
                    files.append(path)
            continue
        if not line[:1] in "+-" or line.startswith("@@"):
            continue
        body = line[1:].strip()
        version_line = re.match(r'^(version\s*=\s*"[^"]*"|\S+\s*=\s*\{.*version\s*=\s*"[^"]*".*\})$', body)
        comment = path and path.endswith(".rs") and body.startswith("//")
        if not (version_line or comment or body == ""):
            other.append(path)
    if not files:
        return (f"The engine sources (`floyd/`, `cargo-floyd/`, `Cargo.toml`, `Cargo.lock`) of"
                f" {this} are identical to `{sweep_rev[:12]}`.")
    listed = ", ".join(f"`{f}`" for f in files)
    if not other:
        return (f"Between `{sweep_rev[:12]}` and {this}, the engine sources (`floyd/`,"
                f" `cargo-floyd/`, `Cargo.toml`, `Cargo.lock`) differ only in version numbers"
                f" and comments ({listed}), so the sweep ran the same engine code as {same}.")
    bad = ", ".join(f"`{f}`" for f in sorted(set(other)))
    return (f"**The engine sources of {this} differ from `{sweep_rev[:12]}` in code** ({bad});"
            " the table describes the older engine.")


def render(sweep):
    names = sorted(
        n for n in os.listdir(sweep)
        if os.path.isfile(os.path.join(sweep, n, "out", "span.json"))
    )
    if not names:
        sys.exit(f"no <crate>/out/span.json under {sweep}")
    crates = [load_crate(sweep, n) for n in names]
    failed = [c for c in crates if c["verdict"] == "build_failed"]
    built = [c for c in crates if c["verdict"] != "build_failed"]

    # A row whose keys are contained in another row's keys (same commit) is
    # covered by that row's workspace run: keep it, exclude it from totals.
    included_in = {}
    partial = []
    for b in built:
        for a in built:
            if a is b or a["rev"] != b["rev"] or not b["keys"]:
                continue
            common = len(a["keys"] & b["keys"])
            if common == len(b["keys"]) and len(a["keys"]) > len(b["keys"]):
                included_in[b["name"]] = a["name"]
            elif common and a["name"] < b["name"]:
                partial.append((a, b, common))
    partial = [(a, b, n) for a, b, n in partial
               if included_in.get(a["name"]) != b["name"]
               and included_in.get(b["name"]) != a["name"]]
    ok = [c for c in built if c["name"] not in included_in]

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
    w(f"- **Floyd commit:** `{floyd_rev[:12]}` (the `cargo-floyd` binary the sweep ran)."
      f" {engine_equivalence(floyd_rev)}")
    w(f"- **Toolchain:** `{toolchain}`{tc_note}")
    w(f"- **Sweep date:** {sweep_dates(sweep)}")
    w(f"- **Machine:** {machine(sweep, hosts)}")
    w(f"- **Crates:** {len(crates)} ({len(failed)} did not build: "
      + (", ".join(c["name"] for c in failed) or "none") + ")")
    w("- **Per crate:** `cargo floyd test --format=json --all-candidates <flags> .`"
      " at the pinned commit (crate list: [`tools/yield/crates.tsv`](tools/yield/crates.tsv))")
    w("- **Regenerate this file:** `python3 tools/yield/render_yield.py <sweep-dir>` from the"
      " repo root, where `<sweep-dir>` is the output of"
      " [`tools/yield/sweep.sh`](tools/yield/sweep.sh) (not checked in)")
    w("")
    w("## How to read this")
    w("")
    w("Every count is of **source functions**: Floyd reports one entry per MIR body, and"
      " entries are deduplicated within a crate by (source file, line, function name), so a"
      " function compiled into both the library and a test harness, or instantiated several"
      " times, counts once. Raw MIR-body counts are in a separate table under Notes."
      " *Functions with a decision* is the denominator: functions with a decision Floyd"
      " recovered (*recovered*, the JSON `candidates`) plus functions listed in"
      " `out_of_scope` with a reason other than `no_decision`, i.e. that contain a decision"
      " Floyd declined. `no_decision` functions (assert-only, derived impls, plain enum"
      " matches, `?`/loop plumbing, drop flags) contain no user-written Boolean decision and"
      " are listed separately below. A recovered function is either *analysed* (MC/DC"
      " analysis against the tests' coverage) or *not measurable* (its coverage cannot be"
      " attributed; reasons as in the JSON). Condition columns count every condition of"
      " every analysed function, not only the decision the text report selects."
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
        name = c["name"]
        if name in included_in:
            name = f"{name} (included in {included_in[name]}, not in totals)"
        if c["verdict"] == "build_failed":
            n = len(head) - 4
            row([f"{name} (not in totals)", f"`{c['rev']}`", flags, verdict] + ["-"] * n)
            continue
        row([name, f"`{c['rev']}`", flags, verdict, c["with_decision"], c["recovered"]]
            + [c["declined"].get(k, 0) for k in decl_cols]
            + ([c["residual"]] if show_residual else [])
            + [fmt_breakdown(c["nm"]), c["analysed"]]
            + [c["status"].get(k, 0) for k in stat_cols])

    def total(key, rows=None):
        return sum(c[key] for c in (ok if rows is None else rows))

    def total_counter(key):
        t = collections.Counter()
        for c in ok:
            t.update(c[key])
        return t

    tdecl, tnm, tstat = total_counter("declined"), total_counter("nm"), total_counter("status")
    row([f"**total ({len(ok)} crates)**", "", "", "",
         f"**{total('with_decision')}**", f"**{total('recovered')}**"]
        + [f"**{tdecl.get(k, 0)}**" for k in decl_cols]
        + ([f"**{total('residual')}**"] if show_residual else [])
        + [f"**{fmt_breakdown(tnm)}**", f"**{total('analysed')}**"]
        + [f"**{tstat.get(k, 0)}**" for k in stat_cols])
    w("")

    twd, tan = total("with_decision"), total("analysed")
    share = f"{100.0 * tan / twd:.1f}%" if twd else "n/a"
    w(f"Analysed functions over functions with a decision: {tan} / {twd} = {share}.")
    if not show_residual:
        w("Every recovered function is either analysed or not measurable.")
    w("")

    notes = []
    for b, a in sorted(included_in.items()):
        bc = next(c for c in built if c["name"] == b)
        notes.append(
            f"- `{b}` is a member of the `{a}` workspace at the same commit (`{bc['rev']}`),"
            f" and every one of its {len(bc['keys'])} functions also appears in `{a}`'s run."
            f" Its row is kept for reference and marked \"included in {a}\"; the totals count"
            f" those functions once, under `{a}`.")
    for a, b, common in partial:
        notes.append(
            f"- `{a['name']}` and `{b['name']}` are the same repository at commit"
            f" `{a['rev']}` and share {common} functions; the totals count those twice.")
    for c in failed:
        err = c["error"] or {}
        msg = err.get("message") if isinstance(err, dict) else str(err)
        notes.append(f"- `{c['name']}`: build failed, no analysis (Floyd's error: \"{msg}\")."
                     " Excluded from the totals.")
    mixed = [(c["name"], c["mixed_reason"]) for c in ok if c["mixed_reason"]]
    if mixed:
        notes.append("- Functions whose MIR bodies were declined in one copy and `no_decision`"
                     " in another count as declined: "
                     + ", ".join(f"{n} {v}" for n, v in mixed) + ".")
    conflicts = [(c["name"], c["status_conflicts"]) for c in ok if c["status_conflicts"]]
    if conflicts:
        notes.append("- Analysed functions whose copies report different condition statuses"
                     " (the first copy is counted): "
                     + ", ".join(f"{n} {v}" for n, v in conflicts) + ".")
    w("## Notes")
    w("")
    L.extend(notes)
    w("")
    w("**Raw MIR-body counts (not deduplicated).** For reference only: one per entry in"
      " Floyd's JSON, so copies of one source function count separately. The main table"
      " does not use these.")
    w("")
    h3 = ["crate", "candidates", "analyses", "not measurable", "out_of_scope declined",
          "out_of_scope no_decision", "candidates + declined"]
    w("| " + " | ".join(h3) + " |")
    w("|---|" + "|".join(["---:"] * (len(h3) - 1)) + "|")
    for c in built:
        row([c["name"], c["raw_candidates"], c["raw_analyses"], c["raw_nm"], c["raw_declined"],
             c["raw_no_decision"], c["raw_candidates"] + c["raw_declined"]])
    row([f"**total ({len(ok)} crates, as above)**", f"**{total('raw_candidates')}**",
         f"**{total('raw_analyses')}**", f"**{total('raw_nm')}**",
         f"**{total('raw_declined')}**", f"**{total('raw_no_decision')}**",
         f"**{total('raw_candidates') + total('raw_declined')}**"])
    w("")

    w("## Not in the denominator")
    w("")
    w("`no_decision` functions by detail (deduplicated as above; a function with several"
      " details is counted under the first alphabetically), and CTFE bodies (compile-time"
      " evaluation copies of runtime functions, `out_of_scope_summary.ctfe_bodies_skipped`,"
      " a raw count), which Floyd counts but does not list.")
    w("")
    details = ordered({k for c in built for k in c["no_dec_detail"]}, [])
    h2 = ["crate", "no_decision"] + [f"no_decision: {k}" for k in details] + ["CTFE bodies skipped"]
    w("| " + " | ".join(h2) + " |")
    w("|---|" + "|".join(["---:"] * (len(h2) - 1)) + "|")
    for c in built:
        name = c["name"] + (" (included in %s)" % included_in[c["name"]]
                            if c["name"] in included_in else "")
        row([name, c["no_decision"]] + [c["no_dec_detail"].get(k, 0) for k in details]
            + [c["ctfe"]])
    tdet = total_counter("no_dec_detail")
    row([f"**total ({len(ok)} crates)**", f"**{total('no_decision')}**"]
        + [f"**{tdet.get(k, 0)}**" for k in details] + [f"**{total('ctfe')}**"])
    w("")
    return "\n".join(L).rstrip("\n") + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("sweep", help="sweep directory (contains <crate>/out/span.json)")
    ap.add_argument("-o", "--output", default="YIELD.md",
                    help="output file (default YIELD.md; '-' for stdout)")
    args = ap.parse_args()
    text = render(args.sweep)
    if args.output == "-":
        sys.stdout.write(text)
    else:
        with open(args.output, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)


if __name__ == "__main__":
    main()
