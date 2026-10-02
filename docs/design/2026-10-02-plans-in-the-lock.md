# Plans in the lock: one spec for a pile of files that share it

*Design, 2026-10-02. Status: implemented 2026-10-02 (branch `plans-in-lock`);
measured as §7 asks — see CLAUDE.md, "Plans in the lock". The slice
`2026-10-02-json-records.md` deferred; this page is where it lands.*

## 1. Why, measured

`villagerdb`'s 7,443 item documents fit one drafted target of 121 columns. Each
member gets a sidecar of 47 KB — 348 MB in all — and they are **one spec**:
ignoring the per-file fingerprint (`[source]`), the timestamp in `[provenance]`
and one note line, all 7,443 are identical. (41% of each file is the same
14-entry `na_values` list repeated on 113 columns.) What that costs, release
build, measured by phase:

| | wall | of which parsing sidecar TOML |
|---|---|---|
| `count(*)` over `dataset()` | 24.0 s, 1.15 GB | 14.5–15.7 s (61%) |
| refit | 32 s, 1.44 GB | 15.0 s (47%) |
| console `.ls` | 17–18 s | 84% |
| first fit | 95 s, 1.39 GB | — (writing sidecars: 31 s; planning: ~50 s) |

Hashing every member for drift is 0.3 s; reading the data is under a second.
Peak memory follows the specs held, not the data: every resolved member owns
its own copy.

## 2. The declaration

```sql
CREATE TABLE items ( … ) WITH (files = '*.json', plans = 'lock');
```

`plans` says where a member's plan is kept: `'sidecars'` (the default, and
everything tdy did before this page) or `'lock'`. It is a fact about storage,
not about meaning, so it is **not** part of `target_hash`: switching it voids
no proof. It is refused with any other value.

Nothing changes for a target that does not declare it.

## 3. What `plans = 'lock'` does

**`tdy fit`** plans every member exactly as it does today — the same sniff, the
same elimination over frames, the same gates and whole-file verification; there
is no "try the pile's spec first" shortcut, because that would skip the
elimination that refuses an ambiguous frame. What changes is where the result
goes:

- a planned member writes **no sidecar**. Its `ParseSpec`, without `notes`, is
  recorded once in the lock's spec table, keyed by the blake3 of its canonical
  serialisation, and the member's lock entry names it:

  ```toml
  lock_version = 2

  [[spec]]
  id = "b3:9c1f…"
  method = "heuristic"          # the provenance every member sharing it has
  tool_version = "0.4.0"
  # … the ParseSpec, as a sidecar's [spec] table …

  [[member]]
  path = "acorn.json"
  blake3 = "…"
  bytes = 173
  spec = "b3:9c1f…"
  notes = ["frame proved by elimination: of 2 candidate frames, …"]
  ```

- per-member `notes` live on the member entry (skipped when empty): they are
  what a refit and the workbench show for that member, and they are the only
  part of a plan that differed across the 7,443.
- a member that has a sidecar file is read from it, as today, and its lock
  entry carries no `spec`: a `manual` sidecar is the way to give one member a
  different plan, and it keeps every rule it has (reused, re-proved, never
  overwritten). A tool-written sidecar left from before the declaration is
  reused the same way; `tdy fit --prune-sidecars` moves each one whose spec the
  lock would record identically into the lock and deletes the file, printing how
  many it removed and how many it kept and why. Nothing is ever deleted without
  that flag.
- the all-or-nothing rule holds: no lock, hence no recorded plan, unless every
  member fits.

**A refit** takes each member's spec from the lock instead of parsing a file,
re-proves it against the target once per distinct spec (conformance costs no
I/O and is a property of the spec), and dry-runs each member as today. A member
whose bytes changed is drift, as today, and is re-planned.

**`dataset()`** loads the lock, parses and validates each distinct spec once,
proves it against the target once, and reads the members in lock order holding
one shared copy (`Arc<ParseSpec>`). It still never plans, never expands a glob
and never writes. The per-member hash for drift stays — it is what makes a
changed file a refusal — and stays cheap.

**Acceptance** is unchanged in meaning: a member's `review` and `accepted` are
in its lock entry; the digest an acceptance is tied to is the spec id for a
lock-held plan (the sidecar file's digest for a sidecar-held one), so a changed
plan expires it exactly as a changed sidecar does.

## 4. Everything that asks for "this member's spec"

One function answers it — the member's sidecar when the file exists, else the
spec its lock entry names — and every caller goes through it:
`report::fit_pile` (reuse), `dataset::resolve`, `lockfile::spec_digest_for`,
`commands` (`tdy check TARGET --against FILE`; `tdy validate FILE` on a member
with no sidecar says its plan is held in `TARGET`'s lock), the console's
`.accept` evidence, `profile` (a member reference resolves through it), and the
workbench (a member with a lock-held plan has no sidecar to watch or to open in
`$EDITOR`; the member view says where its plan is and that writing a sidecar
overrides it).

`messy('file')` is untouched: it is a query about a file, not a member, and a
file with no sidecar is sniffed as it always was.

`lock_version = 2` is written only when the lock holds a spec table; a lock
without one stays version 1 and reads as before. A tdy that does not know
version 2 refuses it — though not by number, as this page first said: 0.3.1
reads the whole lock before the version and fails on the first field it does
not know, "… is not a valid lock file: TOML parse error at line 14, column 3 …
unknown field `spec`, expected one of `lock_version`, `target`, …" (and on a
target that declares the option, earlier: "unknown WITH option `plans`"). This
build and later read the version first and refuse an unknown one by number.

## 5. Hints, not surprises

- `tdy draft` writes `plans = 'lock'` into the `WITH` clause, with a one-line
  comment, when the pile it drafted from has 200 files or more.
- `tdy fit` on a `plans = 'sidecars'` target that just wrote 200 or more
  sidecars holding one plan says so once, naming the option.

## 6. What this does not do

It does not change what a plan is, how one is proved, or which members a pile
has. It does not coalesce one-row members into larger batches — the measured
4.6 s of per-member batch building in the query stays, and is the next thing to
look at. It does not share a plan across targets.

## 7. Tests and the measure of success

- unit: spec identity ignores `notes` and nothing else; `plans` parses, is
  refused on other values, and leaves `target_hash` unchanged
  (`a_target_without_the_new_options_hashes_as_0_2_0_did` keeps passing);
- `tests/dataset.rs` / `tests/fit.rs`: the drifting-exports pile under
  `plans = 'lock'` fits with no sidecar written, the lock carries the specs, the
  query total is 57,340.00 over 36 rows — the same number as with sidecars; a
  `manual` sidecar for one member is used and kept; drift on one file refuses
  the query and a refit re-plans only that member; an acceptance survives a
  refit and expires when the member's plan changes; `--prune-sidecars` moves
  identical tool-written sidecars and keeps a hand-edited one; a version-2 lock
  whose spec table is edited to a spec that no longer conforms is refused by
  `dataset()`; a version-1 lock still reads;
- the console, `check --against`, `validate`, `profile` and the workbench member
  view on a lock-held member, each with a pinned message;
- the corpus pile, re-measured: the same four rows as §1, plus the row count
  (7,443) and the four sums the JSON-records slice pinned, equal under both
  storages.
