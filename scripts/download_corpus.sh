#!/usr/bin/env bash
# Download a curated corpus of data-munging / data-wrangling exercises.
#
# Goal: collect sources that pair exercise/problem statements with datasets,
# while preserving each upstream repository unchanged inside a consistent
# local scaffold.
#
# Usage:
#   chmod +x download_data_munging_exercises.sh
#   ./download_data_munging_exercises.sh [TARGET_DIR]
#
# Environment switches:
#   WITH_SUPPLEMENTAL=1   include broader course/data pools (default: 1)
#   UPDATE_EXISTING=1     git pull existing clones (default: 0)
#   GIT_DEPTH=1           shallow-clone depth (default: 1; set 0 for full history)
#
# Example:
#   WITH_SUPPLEMENTAL=0 ./download_data_munging_exercises.sh ./munging-corpus

set -Eeuo pipefail

ROOT="${1:-data-munging-exercises}"
WITH_SUPPLEMENTAL="${WITH_SUPPLEMENTAL:-1}"
UPDATE_EXISTING="${UPDATE_EXISTING:-0}"
GIT_DEPTH="${GIT_DEPTH:-1}"

log()  { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mWARN:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }

command -v git >/dev/null 2>&1 || die "git is required"
command -v find >/dev/null 2>&1 || die "find is required"
command -v awk >/dev/null 2>&1 || die "awk is required"

mkdir -p "$ROOT"/{00_catalog,01_direct_fit,02_messy_cleaning,03_course_labs,04_pipeline_wrangling,90_supplemental}

clone_repo() {
  local url="$1"
  local dest="$2"

  mkdir -p "$(dirname "$dest")"

  if [[ -d "$dest/.git" ]]; then
    if [[ "$UPDATE_EXISTING" == "1" ]]; then
      log "Updating $(basename "$dest")"
      if ! git -C "$dest" pull --ff-only; then
        warn "Could not update $dest; leaving existing clone intact."
      fi
    else
      printf 'skip  %s\n' "$dest"
    fi
    return 0
  fi

  if [[ -e "$dest" ]]; then
    warn "$dest exists but is not a git checkout; skipping it."
    return 0
  fi

  log "Cloning $url"
  if [[ "$GIT_DEPTH" == "0" ]]; then
    git clone "$url" "$dest" || warn "Clone failed: $url"
  else
    git clone --depth "$GIT_DEPTH" "$url" "$dest" || warn "Clone failed: $url"
  fi
}

write_pack_note() {
  local pack="$1"
  local title="$2"
  local fit="$3"
  local note="$4"

  mkdir -p "$pack"
  cat > "$pack/EXERCISE_PACK.md" <<NOTE
# $title

**Fit for this corpus:** $fit

$note

The upstream source is kept in one or more subdirectories of this folder. Check the upstream README and LICENSE files before redistributing or adapting material.
NOTE
}

# -----------------------------------------------------------------------------
# Catalog / manifest
# -----------------------------------------------------------------------------
cat > "$ROOT/00_catalog/SOURCES.tsv" <<'TSV'
tier	id	problem_statements	data_pairing	solutions	primary_source	notes
DIRECT	practiceprobs	strong	strong	web/partial	https://github.com/practiceprobs/problemsets	Problem repo plus separate dataset repo; concise goal-oriented problems.
DIRECT	pandas_exercises	strong	strong	strong	https://github.com/guipsamora/pandas_exercises	Large classic exercise bank; many datasets; exercise and solution notebooks.
DIRECT	cap3321c	strong	strong	strong	https://github.com/c-marq/CAP3321C-Data-Wrangling	Dedicated Python data-wrangling course with exercises, labs, case studies, data and solutions.
DIRECT	ben519_datawrangling	strong	strong	strong	https://github.com/ben519/DataWrangling	Objectives-only files plus answers; four linked relational-style datasets.
DIRECT	completejourney	strong	strong	strong	https://github.com/GCOM7140/completejourney-exercises	Explicit wrangling exercises; dataset is supplied via the companion completejourney R package.
DIRECT	dlab_r_wrangling	strong	strong	strong	https://github.com/dlab-berkeley/R-Data-Wrangling-Legacy	Challenge problems, datasets and solution folder.
DIRECT	rafalab_ds4stats	strong	strong	strong	https://github.com/rafalab/ds4stats	Labs + data + solutions, including data-wrangling material.
DIRECT	python_structured_data	strong	strong	partial	https://github.com/kwaldenphd/python-structured-data	University lab with notebook questions and structured-data files; duplicates/missing-data work.
DIRECT	meds_assignment1	strong	strong	autograder	https://github.com/MEDS-eds-232/assignment-1	Self-contained assignment: task notebook + real lake-survey CSV + tests.
MESSY	oxford_messy_data	medium	strong	worked_scripts	https://github.com/OxfordIHTM/messy-data	Purpose-built messy XLSX datasets illustrating common tidiness/data-entry problems.
MESSY	openrefine_ecology	strong	strong	lesson_walkthrough	https://github.com/datacarpentry/OpenRefine-ecology-lesson	Intentionally corrupted real ecology data plus explicit cleaning challenges.
MESSY	openrefine_socialsci	strong	strong	lesson_walkthrough	https://github.com/datacarpentry/openrefine-socialsci	OpenRefine social-science cleaning exercises and lesson data.
MESSY	spreadsheet_ecology	strong	strong	lesson_walkthrough	https://github.com/datacarpentry/spreadsheet-ecology-lesson	Messy spreadsheet data plus QC, sorting, dates and organization exercises.
MESSY	librarycarpentry_openrefine	strong	strong	lesson_walkthrough	https://github.com/LibraryCarpentry/lc-open-refine	Bibliographic messy-data exercises; CC instructional material.
MESSY	jhu_openrefine	strong	strong	workshop_guide	https://github.com/jhu-data-services/data-cleaning-openrefine	Raw user-entered UFO data plus a workshop guide and transformations.
MESSY	lissertations_openrefine	strong	strong	walkthrough	https://github.com/lissertations/openrefine	'Dodgy' bibliographic datasets with concrete things-to-try cleaning tasks.
PIPELINE	data_engineering_practice	strong	mixed	partial	https://github.com/danielbeach/data-engineering-practice	Exercise-per-folder practice, including CSV/JSON, downloads, pandas and dirty-data pipeline work.
PIPELINE	dand_wrangling	strong	mixed	worked_projects	https://github.com/marcellovictorino/DAND_4_Data_Wrangling	Udacity-derived gather/assess/clean projects with several real-world datasets.
SUPPLEMENT	datascience_box	strong	mixed	mixed	https://github.com/tidyverse/datascience-box	Large open course with labs/homework/starters; not all are munging-focused or locally data-bundled.
SUPPLEMENT	tidytuesday	weak	strong	community	https://github.com/rfordatascience/tidytuesday	Huge real-data pool; weekly context but usually no fixed target answer.
SUPPLEMENT	python_ecology	strong	strong	lesson_walkthrough	https://github.com/datacarpentry/python-ecology-lesson	Carpentries Python data analysis/wrangling lesson with ecology data and challenges.
SUPPLEMENT	r_ecology	strong	strong	lesson_walkthrough	https://github.com/datacarpentry/R-ecology-lesson	Carpentries R data cleaning/manipulation challenges with ecology data.
SUPPLEMENT	tidy_data_python	medium	strong	worked_notebooks	https://github.com/webartifex/tidy-data	Python implementation of the canonical five messy-data patterns plus case study.
SUPPLEMENT	pandas_problem_solving	strong	strong	inline	https://github.com/maryamfarooq13/Pandas-Problem-solving	Guided notebooks, case studies and local datafiles; useful extra volume.
TSV

cat > "$ROOT/README.md" <<'README'
# Data Munging Exercise Corpus

This directory is generated by `download_data_munging_exercises.sh`.

The corpus is intentionally split by how closely a source matches the desired unit:

1. **01_direct_fit/** — explicit problem/exercise statements paired with data.
2. **02_messy_cleaning/** — intentionally dirty/messy datasets paired with cleaning tasks.
3. **03_course_labs/** — university/workshop labs with prompts + datasets + often solutions/tests.
4. **04_pipeline_wrangling/** — broader gather/clean/transform pipeline exercises.
5. **90_supplemental/** — rich sources worth mining, but not every item is a self-contained munging exercise.

Each exercise pack contains an `EXERCISE_PACK.md` metadata note and one or more untouched upstream git checkouts.

Generated indexes after download:

- `00_catalog/SOURCES.tsv` — curated source-level manifest.
- `00_catalog/DATA_FILES.txt` — likely raw/input datasets found in the downloaded corpus.
- `00_catalog/EXERCISE_FILES.txt` — likely files containing prompts/instructions/labs.
- `00_catalog/SOLUTION_FILES.txt` — likely solutions/answers/tests.
- `00_catalog/REPO_COMMITS.tsv` — exact checked-out commit for reproducibility.

The downloader does **not** imply that all upstream material has the same reuse license. Consult each upstream repository's LICENSE/README before redistribution.
README

# -----------------------------------------------------------------------------
# 01_direct_fit — best matches to "goal/problem statement + dataset(s)"
# -----------------------------------------------------------------------------

pack="$ROOT/01_direct_fit/practiceprobs"
write_pack_note "$pack" "Practice Probs" "Excellent" \
  "Problem statements are in the problemsets repo; the companion datasets repo contains CSVs used by many problems. This is one of the cleanest language-agnostic prompt+data sources. Solutions are generally hosted on the Practice Probs site rather than fully mirrored in GitHub."
clone_repo https://github.com/practiceprobs/problemsets.git "$pack/problemsets"
clone_repo https://github.com/practiceprobs/datasets.git "$pack/datasets"

pack="$ROOT/01_direct_fit/pandas_exercises"
write_pack_note "$pack" "guipsamora/pandas_exercises" "Excellent" \
  "Large exercise bank organized by filtering/sorting, grouping, apply, merge, time series, indexing and more. Exercise notebooks and solution notebooks are paired with local or referenced datasets."
clone_repo https://github.com/guipsamora/pandas_exercises.git "$pack/source"

pack="$ROOT/01_direct_fit/ben519_datawrangling"
write_pack_note "$pack" "ben519/DataWrangling" "Excellent" \
  "Especially useful because the blank R/Python files contain objectives only, while companion files contain answers. Four related CSVs (products, sessions, transactions, users) support many relational wrangling tasks."
clone_repo https://github.com/ben519/DataWrangling.git "$pack/source"

pack="$ROOT/01_direct_fit/completejourney"
write_pack_note "$pack" "Complete Journey exercises" "Excellent" \
  "Exercises and solutions use a realistic grocery-retail relational dataset. The exercise repo does not vendor the data, so this pack also clones the upstream completejourney data package."
clone_repo https://github.com/GCOM7140/completejourney-exercises.git "$pack/exercises"
clone_repo https://github.com/bradleyboehmke/completejourney.git "$pack/data-package"

# -----------------------------------------------------------------------------
# 02_messy_cleaning — sources deliberately constructed around dirty data
# -----------------------------------------------------------------------------

pack="$ROOT/02_messy_cleaning/oxford_messy_data"
write_pack_note "$pack" "Oxford IHTM messy-data" "Excellent for messy inputs" \
  "Contains real and synthetic XLSX examples of headers-as-values, variables packed into one field, multiple tables/sheets, and data-entry errors, together with cleaning scripts. Problem statements are more lesson-like than autograded."
clone_repo https://github.com/OxfordIHTM/messy-data.git "$pack/source"

pack="$ROOT/02_messy_cleaning/openrefine_ecology"
write_pack_note "$pack" "Data Carpentry OpenRefine Ecology" "Excellent" \
  "Uses ecology data intentionally modified with errors to demonstrate clustering, typo repair, missingness, splitting and reconciliation. The lesson episodes contain explicit challenges."
clone_repo https://github.com/datacarpentry/OpenRefine-ecology-lesson.git "$pack/source"

pack="$ROOT/02_messy_cleaning/openrefine_socialsci"
write_pack_note "$pack" "Data Carpentry OpenRefine Social Sciences" "Excellent" \
  "Two-hour cleaning lesson with task-oriented episodes and social-science data. Useful even if exercises are reimplemented in R/Python rather than OpenRefine."
clone_repo https://github.com/datacarpentry/openrefine-socialsci.git "$pack/source"

pack="$ROOT/02_messy_cleaning/spreadsheet_ecology"
write_pack_note "$pack" "Data Carpentry Spreadsheet Ecology" "Excellent" \
  "Designed around deliberately messy spreadsheet data and concrete tasks involving table organization, data validation, sorting, dates, quality control and export."
clone_repo https://github.com/datacarpentry/spreadsheet-ecology-lesson.git "$pack/source"

pack="$ROOT/02_messy_cleaning/librarycarpentry_openrefine"
write_pack_note "$pack" "Library Carpentry OpenRefine" "Excellent" \
  "Bibliographic/metadata-oriented data cleaning exercises. Particularly good for inconsistent strings, clustering, splitting fields and reconciliation."
clone_repo https://github.com/LibraryCarpentry/lc-open-refine.git "$pack/source"

pack="$ROOT/02_messy_cleaning/jhu_openrefine"
write_pack_note "$pack" "JHU Data Cleaning in OpenRefine" "Very good" \
  "Ships raw user-entered UFO sighting data together with a workshop guide and hands-on transformations. A good source of naturally inconsistent real-world strings, dates and categorical values."
clone_repo https://github.com/jhu-data-services/data-cleaning-openrefine.git "$pack/source"

pack="$ROOT/02_messy_cleaning/lissertations_openrefine"
write_pack_note "$pack" "VALA / lissertations OpenRefine project" "Very good" \
  "Explicitly presents 'dodgy data' and a list of concrete cleaning tasks against bibliographic CSV/TSV inputs."
clone_repo https://github.com/lissertations/openrefine.git "$pack/source"

# -----------------------------------------------------------------------------
# 03_course_labs — substantial lab/course repositories with prompt+data pairs
# -----------------------------------------------------------------------------

pack="$ROOT/03_course_labs/cap3321c_python_wrangling"
write_pack_note "$pack" "CAP3321C Data Wrangling with Python" "Excellent" \
  "A dedicated data-wrangling course with separate exercises, labs, case studies, shared data and solutions directories. High-value source for extracting many self-contained assignments."
clone_repo https://github.com/c-marq/CAP3321C-Data-Wrangling.git "$pack/source"

pack="$ROOT/03_course_labs/dlab_r_data_wrangling"
write_pack_note "$pack" "Berkeley D-Lab R Data Wrangling" "Very good" \
  "Workshop scripts contain challenge problems and the repository has a solutions folder. Useful for concise tasks spanning filtering, reshaping, grouping and joins."
clone_repo https://github.com/dlab-berkeley/R-Data-Wrangling-Legacy.git "$pack/source"

pack="$ROOT/03_course_labs/rafalab_ds4stats"
write_pack_note "$pack" "Data Science for Statisticians" "Very good" \
  "Contains labs, solutions and data in separate directories. Not every lab is munging-only, but the structure makes prompt/data/solution extraction straightforward."
clone_repo https://github.com/rafalab/ds4stats.git "$pack/source"

pack="$ROOT/03_course_labs/python_structured_data"
write_pack_note "$pack" "Notre Dame Structured Data & Pandas Lab" "Very good" \
  "Lab questions are paired with CSV/JSON examples and explicitly cover sorting/filtering, parsing issues, duplicates and missing data."
clone_repo https://github.com/kwaldenphd/python-structured-data.git "$pack/source"

pack="$ROOT/03_course_labs/meds_assignment1"
write_pack_note "$pack" "EDS 232 Assignment 1" "Good compact assignment" \
  "Task 1 is a self-contained pandas wrangling assignment using a real freshwater lake survey CSV and bundled autograder tests."
clone_repo https://github.com/MEDS-eds-232/assignment-1.git "$pack/source"

# -----------------------------------------------------------------------------
# 04_pipeline_wrangling — broader data-engineering / gather-assess-clean tasks
# -----------------------------------------------------------------------------

pack="$ROOT/04_pipeline_wrangling/data_engineering_practice"
write_pack_note "$pack" "Data Engineering Practice Problems" "Very good for pipelines" \
  "Exercise-per-directory problems cover file downloads, CSV/JSON/Parquet, pandas, cleansing, databases and PySpark. Some exercises fetch their data at run time rather than shipping every input locally."
clone_repo https://github.com/danielbeach/data-engineering-practice.git "$pack/source"

pack="$ROOT/04_pipeline_wrangling/dand_data_wrangling"
write_pack_note "$pack" "Udacity DAND Data Wrangling exercises" "Good project-style source" \
  "Gather-assess-clean projects around Armenian jobs, Rotten Tomatoes and WeRateDogs. Useful for longer, realistic munging specifications; some upstream data dependencies may need separate retrieval."
clone_repo https://github.com/marcellovictorino/DAND_4_Data_Wrangling.git "$pack/source"

# -----------------------------------------------------------------------------
# 90_supplemental — broader pools worth mining but not uniformly prompt+data
# -----------------------------------------------------------------------------
if [[ "$WITH_SUPPLEMENTAL" == "1" ]]; then
  pack="$ROOT/90_supplemental/datascience_box"
  write_pack_note "$pack" "Data Science in a Box" "Broad supplemental source" \
    "Large open course with labs, homework assignments and starter materials. Many exercises are useful, but not every task is munging-focused and some data arrive via R packages."
  clone_repo https://github.com/tidyverse/datascience-box.git "$pack/source"

  pack="$ROOT/90_supplemental/tidytuesday"
  write_pack_note "$pack" "TidyTuesday" "Dataset/context pool rather than fixed exercises" \
    "Hundreds of real datasets with weekly context and metadata. Best used to author additional explicit munging goals; most weeks do not prescribe a single target transformation."
  clone_repo https://github.com/rfordatascience/tidytuesday.git "$pack/source"

  pack="$ROOT/90_supplemental/carpentries_python_ecology"
  write_pack_note "$pack" "Data Carpentry Python Ecology" "Strong supplemental course" \
    "Data analysis and visualization lesson with numerous challenges and ecology datasets; includes wrangling but also broader programming/visualization material."
  clone_repo https://github.com/datacarpentry/python-ecology-lesson.git "$pack/source"

  pack="$ROOT/90_supplemental/carpentries_r_ecology"
  write_pack_note "$pack" "Data Carpentry R Ecology" "Strong supplemental course" \
    "R lesson with data-frame cleaning, filtering and mutation challenges; broader than pure data munging."
  clone_repo https://github.com/datacarpentry/R-ecology-lesson.git "$pack/source"

  pack="$ROOT/90_supplemental/tidy_data_python"
  write_pack_note "$pack" "Tidy Data in Python" "Good pattern library" \
    "Notebooks implement the five canonical messy-data structures from Wickham's Tidy Data paper. More worked-example than problem bank, but excellent source material for synthetic exercises."
  clone_repo https://github.com/webartifex/tidy-data.git "$pack/source"

  pack="$ROOT/90_supplemental/pandas_problem_solving"
  write_pack_note "$pack" "Pandas Problem Solving" "Additional volume" \
    "Guided pandas notebooks, mini-projects and local datafiles. Included as a lower-priority source because it is less established than the direct-fit collections."
  clone_repo https://github.com/maryamfarooq13/Pandas-Problem-solving.git "$pack/source"
fi

# -----------------------------------------------------------------------------
# Build useful post-download inventories without modifying upstream repos.
# -----------------------------------------------------------------------------
log "Building corpus inventories"

find "$ROOT" \
  -type d -name .git -prune -o \
  -type f \( \
    -iname '*.csv' -o -iname '*.tsv' -o -iname '*.xlsx' -o -iname '*.xls' \
    -o -iname '*.json' -o -iname '*.jsonl' -o -iname '*.parquet' -o -iname '*.feather' \
    -o -iname '*.rds' -o -iname '*.rda' -o -iname '*.rdata' -o -iname '*.dta' \
    -o -iname '*.sav' -o -iname '*.sas7bdat' -o -iname '*.zip' -o -iname '*.gz' \
  \) -print | sort > "$ROOT/00_catalog/DATA_FILES.txt"

find "$ROOT" \
  -type d -name .git -prune -o \
  -type f \( \
    -iname '*.md' -o -iname '*.qmd' -o -iname '*.rmd' -o -iname '*.ipynb' \
    -o -iname '*.pdf' -o -iname '*.docx' -o -iname '*.html' -o -iname '*.py' -o -iname '*.r' \
  \) -print | \
  awk 'BEGIN{IGNORECASE=1} /exercise|problem|assignment|challenge|lab|homework|worksheet|starter|task|practice/' | \
  sort -u > "$ROOT/00_catalog/EXERCISE_FILES.txt"

find "$ROOT" \
  -type d -name .git -prune -o \
  -type f -print | \
  awk 'BEGIN{IGNORECASE=1} /solution|answer|key|autograder|tests?\//' | \
  sort -u > "$ROOT/00_catalog/SOLUTION_FILES.txt"

{
  printf 'path\tcommit\tremote\n'
  while IFS= read -r gitdir; do
    repo="${gitdir%/.git}"
    commit="$(git -C "$repo" rev-parse HEAD 2>/dev/null || printf 'unknown')"
    remote="$(git -C "$repo" remote get-url origin 2>/dev/null || printf 'unknown')"
    printf '%s\t%s\t%s\n' "$repo" "$commit" "$remote"
  done < <(find "$ROOT" -type d -name .git -print | sort)
} > "$ROOT/00_catalog/REPO_COMMITS.tsv"

# Quick summary
nrepos=$(( $(wc -l < "$ROOT/00_catalog/REPO_COMMITS.tsv") - 1 ))
ndata=$(wc -l < "$ROOT/00_catalog/DATA_FILES.txt")
nexercise=$(wc -l < "$ROOT/00_catalog/EXERCISE_FILES.txt")
nsolution=$(wc -l < "$ROOT/00_catalog/SOLUTION_FILES.txt")

cat > "$ROOT/00_catalog/SUMMARY.txt" <<SUMMARY
Downloaded repository checkouts: $nrepos
Likely dataset/archive files:     $ndata
Likely exercise/prompt files:     $nexercise
Likely solution/test files:       $nsolution

These counts are heuristic. Inspect SOURCES.tsv and each EXERCISE_PACK.md for curation notes.
SUMMARY

log "Done"
printf '\nCorpus root: %s\n' "$ROOT"
printf 'Catalog:     %s/00_catalog/SOURCES.tsv\n' "$ROOT"
printf 'Summary:     %s/00_catalog/SUMMARY.txt\n' "$ROOT"
printf '\nTip: run with WITH_SUPPLEMENTAL=0 for only the tighter corpus.\n'
