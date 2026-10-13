# Ground-truth accuracy study — sampling & labeling plan (Extension E3)

Purpose: the one number a CleverCSV/HypoParsr-literate reviewer will ask for —
sniff-tier accuracy against human-verified ground truth on real files —
reported the way CleverCSV's Tables 1–2 report dialect accuracy (overall + a
"messy" split). Closes D2's "one gap worth addressing pre-submission."

## What is labeled, per file
For each sampled file, a human records (10–60 seconds/file with the harness):
1. delimiter (CSV only) — correct / wrong / n.a.
2. header row(s) chosen — correct / wrong
3. title/metadata rows skipped — correct / wrong / none present
4. per-column dtypes — all correct / list the wrong ones
5. verdict: SPEC CORRECT (all of 1–4) / SPEC WRONG (name the field) /
   AMBIGUOUS (two defensible readings — excluded from accuracy, counted
   separately, mirroring CleverCSV's exclusion rule but REPORTED not hidden)

## Sample design
- Frame: the 9,881-file corpus (26 repos), stratified by format from the
  per-format breakdown (corpus_survey.out, running).
- n = 150: 90 CSV (weighted toward files the sweep marks low-confidence —
  oversampling the hard tail the way CleverCSV's messy split does), 30 Excel
  (.xlsx/.xls), 15 JSON/NDJSON, 15 other (logs/fixed-width/ODS).
- Selection: seeded random within strata (seed 20260903), so the sample is
  reproducible from the corpus manifest.

## Harness (exists already; one small script to write)
`scripts/label_sample.sh` (to create): for each sampled path, run
`tdy sniff FILE --no-llm --json`, render `.show`-style head beside the parsed
preview, prompt the labeler for verdicts 1–5, append one JSON line to
`gap_reports/labels.jsonl`. Everything needed (sniff --json, raw head,
preview) ships in the installed binary today.

## Reporting (goes into §2.4 of the paper)
- Overall: % of non-ambiguous files with SPEC CORRECT.
- Messy split: same, restricted to files the sniffer itself marked
  confidence < 0.9 (tdy's own hard tail — the honest analogue of
  CleverCSV's messy split).
- Refusal audit: for every corpus file tdy declined or refused, was the
  refusal justified (file genuinely ambiguous/unreadable)? This is the
  refusal-correctness metric no competitor reports — white space D7 named.
- Ambiguity rate, reported not excluded silently.

## Cost estimate
150 files × ~45 s median = ~2 hours of labeling once the harness exists;
harness ~1 hour to write. Total ≈ half a person-day, below D7's 1–2 day
upper bound because the harness shows sniff output beside the raw head
instead of requiring manual file inspection.

## What this does NOT claim
Not a head-to-head with CleverCSV (different corpus, different task scope —
tdy labels the whole spec, not just dialect). The paper's "not a horse race"
stance stands; this adds the missing absolute number, not a comparison.
