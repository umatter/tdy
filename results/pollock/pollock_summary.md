# tdy on Pollock — `polluted_files`

2290 polluted files, each isolating one deviation from RFC 4180. Metrics are Pollock's own `pollock.metrics`, so these rows sit in the same table as the paper's.

`typed` is what `tdy query` returns. `text` is the same structural read with every column left as `utf8` — the benchmark compares cells as exact strings, so any system that parses a date loses cell score for writing it back in ISO form. Pollock ships both flavours of DuckDB for that reason, and the two tdy rows are the same comparison.

| system | success | header F1 | record F1 | cell F1 |
|---|---|---|---|---|
| **tdy (typed)** | 0.999 | 0.957 | 0.991 | 0.873 |
| **tdy (text)** | 0.999 | 0.957 | 0.991 | 0.967 |
| duckdbauto (typed) | 1.000 | 0.978 | 0.919 | 0.792 |
| duckdbparse (all text) | 1.000 | 1.000 | 0.991 | 0.996 |
| pandas | 0.999 | 0.995 | 0.978 | 0.991 |
| clevercsv | 1.000 | 0.975 | 0.840 | 0.904 |
| rhypoparsr | 1.000 | 0.198 | 0.124 | 0.604 |
| postgres | 0.017 | 0.015 | 0.013 | 0.012 |

## tdy (typed), by pollution family

| group | files | success | header p | header r | header f1 | record p | record r | record f1 | cell p | cell r | cell f1 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| **all** | 2290 | 0.999 | 0.986 | 0.933 | 0.957 | 0.991 | 0.991 | 0.991 | 0.899 | 0.851 | 0.873 |
| table (header rows, preamble, several tables) | 12 | 0.917 | 0.667 | 0.658 | 0.662 | 1.000 | 0.868 | 0.913 | 0.966 | 0.831 | 0.876 |
| inconsistent rows (extra/missing separators) | 1428 | 1.000 | 0.988 | 0.933 | 0.959 | 0.996 | 0.996 | 0.996 | 0.895 | 0.847 | 0.870 |
| structural (delimiter, quote, escape, newline) | 848 | 0.999 | 0.988 | 0.936 | 0.959 | 0.984 | 0.986 | 0.985 | 0.904 | 0.859 | 0.879 |
| other | 2 | 0.500 | 0.500 | 0.500 | 0.500 | 0.500 | 0.500 | 0.500 | 0.445 | 0.445 | 0.445 |

- loaded: **2287/2290** (99.9%)
- exactly right on every header, record and cell: **1/2290** (0.0%)

| tdy's own verdict | files | cell F1 | record F1 | header F1 | exactly right |
|---|---|---|---|---|---|
| confident (≥ 0.80) | 2257 | 0.874 | 0.992 | 0.970 | 1/2257 (0%) |
| flagged (< 0.80) | 30 | 0.856 | 0.954 | 0.033 | 0/30 (0%) |

**Wrong while confident: 2256/2257** — the number this project's rule is about.

- `file_multitable_more.csv` — confidence 0.80, cell F1 0.623, header F1 0.947
- `row_extra_quote42_col8.csv` — confidence 0.80, cell F1 0.666, header F1 0.750
- `row_extra_quote43_col8.csv` — confidence 0.80, cell F1 0.666, header F1 0.750
- `file_multitable_less.csv` — confidence 0.80, cell F1 0.669, header F1 1.000
- `row_extra_quote28_col8.csv` — confidence 0.80, cell F1 0.694, header F1 0.783
- `row_extra_quote34_col8.csv` — confidence 0.80, cell F1 0.694, header F1 0.783
- `row_extra_quote35_col8.csv` — confidence 0.80, cell F1 0.694, header F1 0.783
- `row_extra_quote40_col8.csv` — confidence 0.80, cell F1 0.694, header F1 0.783
- `row_extra_quote47_col8.csv` — confidence 0.80, cell F1 0.694, header F1 0.783
- `row_extra_quote55_col8.csv` — confidence 0.80, cell F1 0.694, header F1 0.783
- `row_extra_quote66_col8.csv` — confidence 0.80, cell F1 0.694, header F1 0.783
- `row_extra_quote74_col8.csv` — confidence 0.80, cell F1 0.694, header F1 0.783
- `row_extra_quote81_col8.csv` — confidence 0.80, cell F1 0.694, header F1 0.783
- `row_extra_quote33_col8.csv` — confidence 0.80, cell F1 0.696, header F1 0.783
- `row_extra_quote41_col8.csv` — confidence 0.80, cell F1 0.696, header F1 0.783
- `row_extra_quote62_col8.csv` — confidence 0.80, cell F1 0.696, header F1 0.783
- `row_extra_quote10_col8.csv` — confidence 0.80, cell F1 0.726, header F1 0.818
- `row_extra_quote1_col8.csv` — confidence 0.80, cell F1 0.726, header F1 0.818
- `row_extra_quote23_col8.csv` — confidence 0.80, cell F1 0.726, header F1 0.818
- `row_extra_quote26_col8.csv` — confidence 0.80, cell F1 0.726, header F1 0.818
- …and 2236 more

Worst loaded files (2286 with cell F1 < 1):

- `file_field_delimiter_0x20.csv` — cell F1 0.125 (P 0.110 / R 0.146), record F1 0.000, header F1 0.000
- `file_multitable_more.csv` — cell F1 0.623 (P 1.000 / R 0.453), record F1 0.667, header F1 0.947
- `row_extra_quote42_col8.csv` — cell F1 0.666 (P 0.881 / R 0.535), record F1 0.982, header F1 0.750
- `row_extra_quote43_col8.csv` — cell F1 0.666 (P 0.881 / R 0.535), record F1 0.982, header F1 0.750
- `file_multitable_less.csv` — cell F1 0.669 (P 1.000 / R 0.503), record F1 0.667, header F1 1.000
- `file_multitable_same.csv` — cell F1 0.669 (P 1.000 / R 0.503), record F1 0.667, header F1 1.000
- `row_extra_quote28_col8.csv` — cell F1 0.694 (P 0.881 / R 0.573), record F1 0.982, header F1 0.783
- `row_extra_quote34_col8.csv` — cell F1 0.694 (P 0.881 / R 0.573), record F1 0.982, header F1 0.783
- `row_extra_quote35_col8.csv` — cell F1 0.694 (P 0.881 / R 0.573), record F1 0.982, header F1 0.783
- `row_extra_quote40_col8.csv` — cell F1 0.694 (P 0.881 / R 0.573), record F1 0.982, header F1 0.783
- `row_extra_quote47_col8.csv` — cell F1 0.694 (P 0.881 / R 0.573), record F1 0.982, header F1 0.783
- `row_extra_quote55_col8.csv` — cell F1 0.694 (P 0.881 / R 0.573), record F1 0.982, header F1 0.783
- `row_extra_quote66_col8.csv` — cell F1 0.694 (P 0.881 / R 0.573), record F1 0.982, header F1 0.783
- `row_extra_quote74_col8.csv` — cell F1 0.694 (P 0.881 / R 0.573), record F1 0.982, header F1 0.783
- `row_extra_quote81_col8.csv` — cell F1 0.694 (P 0.881 / R 0.573), record F1 0.982, header F1 0.783
- `row_extra_quote33_col8.csv` — cell F1 0.696 (P 0.882 / R 0.574), record F1 0.982, header F1 0.783
- `row_extra_quote41_col8.csv` — cell F1 0.696 (P 0.882 / R 0.574), record F1 0.982, header F1 0.783
- `row_extra_quote62_col8.csv` — cell F1 0.696 (P 0.882 / R 0.574), record F1 0.982, header F1 0.783
- `row_extra_quote10_col8.csv` — cell F1 0.726 (P 0.881 / R 0.617), record F1 0.982, header F1 0.818
- `row_extra_quote1_col8.csv` — cell F1 0.726 (P 0.881 / R 0.617), record F1 0.982, header F1 0.818
- `row_extra_quote23_col8.csv` — cell F1 0.726 (P 0.881 / R 0.617), record F1 0.982, header F1 0.818
- `row_extra_quote26_col8.csv` — cell F1 0.726 (P 0.881 / R 0.617), record F1 0.982, header F1 0.818
- `row_extra_quote51_col8.csv` — cell F1 0.726 (P 0.881 / R 0.617), record F1 0.982, header F1 0.818
- `row_extra_quote65_col8.csv` — cell F1 0.726 (P 0.881 / R 0.617), record F1 0.982, header F1 0.818
- `row_extra_quote67_col8.csv` — cell F1 0.726 (P 0.881 / R 0.617), record F1 0.982, header F1 0.818
- `row_extra_quote6_col8.csv` — cell F1 0.726 (P 0.881 / R 0.617), record F1 0.982, header F1 0.818
- `row_extra_quote72_col8.csv` — cell F1 0.726 (P 0.881 / R 0.617), record F1 0.982, header F1 0.818
- `row_extra_quote78_col8.csv` — cell F1 0.726 (P 0.881 / R 0.617), record F1 0.982, header F1 0.818
- `row_extra_quote80_col8.csv` — cell F1 0.726 (P 0.881 / R 0.617), record F1 0.982, header F1 0.818
- `row_extra_quote15_col8.csv` — cell F1 0.727 (P 0.882 / R 0.618), record F1 0.982, header F1 0.818
- …and 2256 more (see pollock_rows_typed.csv)

## tdy (text), by pollution family

| group | files | success | header p | header r | header f1 | record p | record r | record f1 | cell p | cell r | cell f1 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| **all** | 2290 | 0.999 | 0.986 | 0.933 | 0.957 | 0.991 | 0.991 | 0.991 | 0.996 | 0.942 | 0.967 |
| table (header rows, preamble, several tables) | 12 | 0.917 | 0.667 | 0.658 | 0.662 | 1.000 | 0.868 | 0.913 | 0.998 | 0.863 | 0.908 |
| inconsistent rows (extra/missing separators) | 1428 | 1.000 | 0.988 | 0.933 | 0.959 | 0.996 | 0.996 | 0.996 | 0.999 | 0.944 | 0.970 |
| structural (delimiter, quote, escape, newline) | 848 | 0.999 | 0.988 | 0.936 | 0.959 | 0.984 | 0.986 | 0.985 | 0.992 | 0.942 | 0.964 |
| other | 2 | 0.500 | 0.500 | 0.500 | 0.500 | 0.500 | 0.500 | 0.500 | 0.500 | 0.500 | 0.500 |

- loaded: **2287/2290** (99.9%)
- exactly right on every header, record and cell: **92/2290** (4.0%)

| tdy's own verdict | files | cell F1 | record F1 | header F1 | exactly right |
|---|---|---|---|---|---|
| confident (≥ 0.80) | 2257 | 0.968 | 0.992 | 0.970 | 92/2257 (4%) |
| flagged (< 0.80) | 30 | 0.948 | 0.954 | 0.033 | 0/30 (0%) |

**Wrong while confident: 2165/2257** — the number this project's rule is about.

- `file_multitable_more.csv` — confidence 0.80, cell F1 0.623, header F1 0.947
- `file_multitable_less.csv` — confidence 0.80, cell F1 0.669, header F1 1.000
- `row_extra_quote42_col8.csv` — confidence 0.80, cell F1 0.748, header F1 0.750
- `row_extra_quote43_col8.csv` — confidence 0.80, cell F1 0.748, header F1 0.750
- `row_extra_quote28_col8.csv` — confidence 0.80, cell F1 0.780, header F1 0.783
- `row_extra_quote34_col8.csv` — confidence 0.80, cell F1 0.780, header F1 0.783
- `row_extra_quote35_col8.csv` — confidence 0.80, cell F1 0.780, header F1 0.783
- `row_extra_quote40_col8.csv` — confidence 0.80, cell F1 0.780, header F1 0.783
- `row_extra_quote47_col8.csv` — confidence 0.80, cell F1 0.780, header F1 0.783
- `row_extra_quote55_col8.csv` — confidence 0.80, cell F1 0.780, header F1 0.783
- `row_extra_quote66_col8.csv` — confidence 0.80, cell F1 0.780, header F1 0.783
- `row_extra_quote74_col8.csv` — confidence 0.80, cell F1 0.780, header F1 0.783
- `row_extra_quote81_col8.csv` — confidence 0.80, cell F1 0.780, header F1 0.783
- `row_extra_quote33_col8.csv` — confidence 0.80, cell F1 0.781, header F1 0.783
- `row_extra_quote41_col8.csv` — confidence 0.80, cell F1 0.781, header F1 0.783
- `row_extra_quote62_col8.csv` — confidence 0.80, cell F1 0.781, header F1 0.783
- `row_extra_quote10_col8.csv` — confidence 0.80, cell F1 0.815, header F1 0.818
- `row_extra_quote1_col8.csv` — confidence 0.80, cell F1 0.815, header F1 0.818
- `row_extra_quote23_col8.csv` — confidence 0.80, cell F1 0.815, header F1 0.818
- `row_extra_quote26_col8.csv` — confidence 0.80, cell F1 0.815, header F1 0.818
- …and 2145 more

Worst loaded files (2195 with cell F1 < 1):

- `file_field_delimiter_0x20.csv` — cell F1 0.125 (P 0.110 / R 0.146), record F1 0.000, header F1 0.000
- `file_multitable_more.csv` — cell F1 0.623 (P 1.000 / R 0.453), record F1 0.667, header F1 0.947
- `file_multitable_less.csv` — cell F1 0.669 (P 1.000 / R 0.503), record F1 0.667, header F1 1.000
- `file_multitable_same.csv` — cell F1 0.669 (P 1.000 / R 0.503), record F1 0.667, header F1 1.000
- `row_extra_quote42_col8.csv` — cell F1 0.748 (P 0.989 / R 0.601), record F1 0.982, header F1 0.750
- `row_extra_quote43_col8.csv` — cell F1 0.748 (P 0.989 / R 0.601), record F1 0.982, header F1 0.750
- `row_extra_quote28_col8.csv` — cell F1 0.780 (P 0.989 / R 0.644), record F1 0.982, header F1 0.783
- `row_extra_quote34_col8.csv` — cell F1 0.780 (P 0.989 / R 0.644), record F1 0.982, header F1 0.783
- `row_extra_quote35_col8.csv` — cell F1 0.780 (P 0.989 / R 0.644), record F1 0.982, header F1 0.783
- `row_extra_quote40_col8.csv` — cell F1 0.780 (P 0.989 / R 0.644), record F1 0.982, header F1 0.783
- `row_extra_quote47_col8.csv` — cell F1 0.780 (P 0.989 / R 0.644), record F1 0.982, header F1 0.783
- `row_extra_quote55_col8.csv` — cell F1 0.780 (P 0.989 / R 0.644), record F1 0.982, header F1 0.783
- `row_extra_quote66_col8.csv` — cell F1 0.780 (P 0.989 / R 0.644), record F1 0.982, header F1 0.783
- `row_extra_quote74_col8.csv` — cell F1 0.780 (P 0.989 / R 0.644), record F1 0.982, header F1 0.783
- `row_extra_quote81_col8.csv` — cell F1 0.780 (P 0.989 / R 0.644), record F1 0.982, header F1 0.783
- `row_extra_quote33_col8.csv` — cell F1 0.781 (P 0.991 / R 0.645), record F1 0.982, header F1 0.783
- `row_extra_quote41_col8.csv` — cell F1 0.781 (P 0.991 / R 0.645), record F1 0.982, header F1 0.783
- `row_extra_quote62_col8.csv` — cell F1 0.781 (P 0.991 / R 0.645), record F1 0.982, header F1 0.783
- `row_extra_quote10_col8.csv` — cell F1 0.815 (P 0.989 / R 0.693), record F1 0.982, header F1 0.818
- `row_extra_quote1_col8.csv` — cell F1 0.815 (P 0.989 / R 0.693), record F1 0.982, header F1 0.818
- `row_extra_quote23_col8.csv` — cell F1 0.815 (P 0.989 / R 0.693), record F1 0.982, header F1 0.818
- `row_extra_quote26_col8.csv` — cell F1 0.815 (P 0.989 / R 0.693), record F1 0.982, header F1 0.818
- `row_extra_quote51_col8.csv` — cell F1 0.815 (P 0.989 / R 0.693), record F1 0.982, header F1 0.818
- `row_extra_quote65_col8.csv` — cell F1 0.815 (P 0.989 / R 0.693), record F1 0.982, header F1 0.818
- `row_extra_quote67_col8.csv` — cell F1 0.815 (P 0.989 / R 0.693), record F1 0.982, header F1 0.818
- `row_extra_quote6_col8.csv` — cell F1 0.815 (P 0.989 / R 0.693), record F1 0.982, header F1 0.818
- `row_extra_quote72_col8.csv` — cell F1 0.815 (P 0.989 / R 0.693), record F1 0.982, header F1 0.818
- `row_extra_quote78_col8.csv` — cell F1 0.815 (P 0.989 / R 0.693), record F1 0.982, header F1 0.818
- `row_extra_quote80_col8.csv` — cell F1 0.815 (P 0.989 / R 0.693), record F1 0.982, header F1 0.818
- `row_extra_quote15_col8.csv` — cell F1 0.816 (P 0.991 / R 0.694), record F1 0.982, header F1 0.818
- …and 2165 more (see pollock_rows_text.csv)
