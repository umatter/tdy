# Adjudication of the 30 crowd-flagged files (2026-09-03)

Method: 4 independent adjudicators, instructed to favour neither tdy nor the raters,
each inspecting the ACTUAL corpus file (head / openpyxl / json / re-running `tdy sniff`)
and ruling on mechanical evidence only.

## Verdicts over the 30 flagged items
| verdict | n | meaning |
|---|---|---|
| CORRECT | 21 | tdy's reading defensible; raters mistaken |
| MISREAD | 5 | genuine defect, named below |
| AMBIGUOUS | 3 | file admits two honest readings (hierarchical gov. reports) |
| DEGRADED-INPUT | 1 | not tabular data; any table reading arbitrary |

## The 5 genuine misreads
1. **country_subdivisions.csv** — encoding detector chose windows-1252 for a file that is
   valid UTF-8 throughout. "Sant Julià de Lòria" reads back "Sant JuliÃ  de LÃ²ria".
   confidence 0.80, NO encoding warning. *** SILENT WRONG VALUE — violates the one rule ***
2. **weather.csv** — decimal scale inferred from first 500 rows (3 dp); a later station's
   153.5456 returns as 153.546. Silently rounded. confidence 0.45 + note warns of rounding.
3. **requirements.txt** — headerless pip list; promote_header consumed line 1, so
   `numpy==1.22.0` survives only inside a column name. A record is dropped. confidence 0.60.
4. **bouldercreek_09_2013.txt** — USGS RDB format-descriptor row (`5s 15s 20d ...`) spliced
   into data, degrading all columns to text. confidence 0.55 + explicit note.
5. **h17.xlsx** — 12 stacked sub-tables read as one flat table; repeated headers pollute
   numeric columns. confidence 0.45 + explicit note.

Pattern: 4 of 5 misreads occur where tdy's own confidence was <= 0.60 and a note fired —
the designed behaviour. #1 is the exception and the only true violation of the governing rule.

## Crowd-label quality (methodological finding)
- Precision of crowd "wrong" verdicts: 5/30 = 17%.
- In most confirmed cases the raters did NOT identify the real defect; adjudicators found it.
- Two systematic noise sources: (a) cross-item contamination — complaints citing columns
  that exist in no version of the file under review; (b) an instrument artifact of ours —
  the panel truncates long column lists with "(+N more)", repeatedly misread as dropped columns.
- CAVEAT: crowd RECALL is unmeasured. The 118 files rated correct were not adjudicated,
  so 5 is a LOWER BOUND on defects in the sample, not a total.
