# The console and the workbench

*Design, 2026-09-01. Status: agreed, not yet built.*

## 1. Why

`tdy-tui` today cannot open without a target: `main.rs` parses a `.tdy.sql`
before it touches the terminal, and `App` is a view over one `PileReport`. That
is the review loop for a declared pile and nothing else. The thing a person
actually has at the start is a directory of files and no declaration, and the
tool that should help them get from one to the other opens with an error.

The CLI has the same shape from the other side: seven subcommands, each a
process, each argument quoted through a shell. Every interactive database
solved this decades ago — `sqlite3`, `psql`, `duckdb` — with one console where
SQL is the default language and everything that is not SQL is a dot-command.
That is what this design adds, and the terminal UI is rebuilt as that console
plus two panes around it.

## 2. What is being built

Three things, in this order, each shippable on its own:

1. **`tdy::console`** — the grammar (`.sniff …`, `.fit …`, bare SQL) and a
   dispatcher that turns a line into a typed `Outcome`. Library code, no
   terminal, no stdout.
2. **The plain console** — `tdy` with no subcommand opens `tdy>` in the
   terminal; with stdin piped it is a batch runner.
3. **The workbench** — `tdy-tui` rebuilt as three panes: a file browser on the
   left, a main pane for understanding the data, the console at the bottom.
   Every action in the UI dispatches a console line, so one code path serves
   both frontends and the scrollback is a faithful record of the session.

Non-goals, stated so they are not drifted into: no embedded shell (a PTY would
give tdy no knowledge of what ran, so the panes could not react); no
`.accept-all` or `--yes`; no second store of state — a session leaves behind
exactly the sidecars, target and lock a CLI session would.

## 3. The grammar

A line beginning with `.` is a dot-command; anything else is SQL. SQL may span
lines and runs when a line ends in `;`. A dot-command is one line. Dot-command
arguments are tokenised like a shell (whitespace-separated, single or double
quotes for paths with spaces) and **globs are expanded by the console**,
relative to its working directory, because no shell stands in front of it.

The command set is the CLI's, one to one, with the CLI's flags:

| console | CLI |
|---|---|
| `.sniff FILE [--quick] [--force] [--no-llm] [--hint "…"]` | `tdy sniff` |
| `.validate FILE [--stamp]` | `tdy validate` |
| `.draft FILES… [--to NAME.tdy.sql]` | `tdy draft` — `--to` replaces the shell redirect; refuses to overwrite |
| `.fit TARGET [FILE] [--dry-run] [--propose]` | `tdy fit` |
| `.check TARGET [--against FILE…]` | `tdy check` |
| `.accept TARGET MEMBER` | `tdy fit TARGET --accept MEMBER` — first-class, because it is *the* judgement (section 8) |
| `SELECT … ;` (any SQL) | `tdy query "…"` |
| `.output FILE [--format parquet\|csv] [--force]` | `tdy query -o` — routes the *next* result to a file, sqlite-style; `.output` alone routes back to the screen |
| `.schema`, `.config init` | same |

Console-only, for orientation without the workbench:

| command | does |
|---|---|
| `.ls [DIR]` | the directory as the browser shows it (section 6): data files and targets with their sidecar/lock status; companions hidden |
| `.cd DIR` | change the working directory, within the root |
| `.show FILE` | the raw head (or a sheet's grid) beside what the sidecar says, if there is one |
| `.edit FILE` | `$EDITOR`; the workbench suspends the alternate screen and redraws on return |
| `.help [CMD]`, `.quit` / `.exit` | as expected; Ctrl-D quits |

**Selection as an implicit argument (workbench only).** `.sniff` with no file
acts on the file highlighted in the browser, and the console **echoes the
completed line** — `.sniff 2025-01.csv` — into the scrollback, so the record
never contains an implicit command. In the plain console the same line is a
parse error naming the missing argument.

## 4. The dispatcher: `tdy::console`

```rust
pub fn parse(line: &str) -> Result<Command, ParseError>;   // pure; unit-tested

pub struct Session { /* cwd, root, Config, SessionContext, output route, sql buffer, history */ }
impl Session {
    pub fn run(&mut self, line: &str, progress: &progress::Sink) -> Outcome;
}

pub struct Outcome {
    pub echo: String,      // the line as actually run: globs and defaults expanded
    pub text: String,      // exactly what the CLI would have printed
    pub payload: Payload,  // the structured part the workbench draws
}

pub enum Payload {
    Nothing,
    Listing(Vec<Entry>),                                   // .ls
    Shown { path: PathBuf, raw: RawHead, spec: Option<SpecSummary> },  // .show
    Sniffed { path: PathBuf, spec: SpecSummary, preview: Preview, notes: Vec<String>, confidence: f32 },
    Drafted { ddl: String, wrote: Option<PathBuf> },
    Fitted(PileReport),                                     // .fit / .check / .accept (second step)
    Evidence { target: PathBuf, member: String, rows: Vec<Evidence> },   // .accept (first step)
    Query(QueryResult),
    Error { message: String, kind: ErrorKind },
}
```

Rules:

- **It calls only the non-printing library functions** — `report::fit_pile`,
  `draft`, the sniff and validate entry points, the SQL provider — never
  anything that writes to stdout. `text` comes from the existing renderers
  (`render_pile_text` and its siblings), so the console's text for `.fit` and
  the CLI's text for `tdy fit` are one function's output, and a test asserts
  it. This is the discipline `mcp.rs` already follows. The intended end state,
  out of scope here, is that the CLI subcommands become thin wrappers over
  `Session::run`.
- **A multi-line SQL statement is buffered in the session** until a line ends
  in `;`; `run` returns `Outcome { payload: Nothing, text: "" }` with a
  continuation marker for the incomplete lines.
- **Long work narrates through `tdy::progress`.** `run` takes a `Sink`, so the
  workbench runs it on a spawned task and the status line says what is
  happening; the plain console prints the same messages on stderr.
- **An error is an `Outcome`, not an `Err`.** It lands in the scrollback like
  any other output and the main pane keeps its context. Only the runtime dying
  propagates.
- **`SessionContext` persists across lines**, so a `messy()` under the caching
  threshold is parsed once per session, not once per query.

## 5. Entry points

| invocation | opens |
|---|---|
| `tdy` (stdin and stdout are TTYs) | the workbench if `tdy-tui` is on PATH; otherwise the plain console, with a one-line note the first time in a session: `terminal UI not installed: cargo install --path tdy-tui` |
| `tdy` (stdin not a TTY) | the batch runner: read lines to EOF, print `text`, exit non-zero at the first error — `tdy < script.tdy` |
| `tdy console` | the plain console, always |
| `tdy ui [PATH]` / `tdy-tui [PATH]` | the workbench, always; with a file as the initial context, with a target also running `.fit` on it (today's behaviour) |
| `tdy <subcommand> …` | unchanged |

The plain console: `tdy>` prompt, `   ->` for continuation lines, in-memory
history on Up/Down, Ctrl-C clears the current line, Ctrl-D quits. No readline
dependency. History is persisted to `~/.local/share/tdy/history` (XDG data
dir), shared with the workbench.

## 6. The workbench frame

```
┌ ~/sales ───────────────┬─ 2025-07.csv ─────────────────────────────────────┐
│ ▸ 2025-01.csv  ✓ 0.95  │                                                   │
│   2025-02.csv  ✓ 0.95  │   main pane: the current context (section 7)      │
│   …                    │                                                   │
│   2025-07.csv  —       │                                                   │
│   2025-09.xlsx ✓ 0.90  │                                                   │
│   sales.tdy.sql 9/12   │                                                   │
│   ▸ archive/           ├─ console ─────────────────────────────────────────┤
│                        │ tdy> .fit sales.tdy.sql                           │
│                        │   2025-07.csv   GAP  amount_chf: no column binds  │
│                        │   …                                               │
│                        │ tdy> █                                            │
├────────────────────────┴───────────────────────────────────────────────────┤
│ fitting 2025-09.xlsx (verifying types)…        Tab focus  ^L zoom  ? keys  │
└────────────────────────────────────────────────────────────────────────────┘
```

**Browser (left, ~26 columns, toggleable).** A tree rooted at the directory the
workbench was started in. Subdirectories can be entered and left, never above
the root: the root is also the confinement boundary for every path the console
accepts (`fileio::confine`, as `tdy mcp --root` uses it). Entries are the files
tdy can read, by extension, plus targets (`*.tdy.sql`). `*.tdy.toml` and
`*.tdy.lock` are never entries; they colour their owner's status column:

| status | meaning |
|---|---|
| `—` | no sidecar |
| `✓ 0.95` | sniffed, with the recorded confidence; red below the escalation threshold |
| `✗ stale` | sidecar fingerprint does not match the file |
| `9/12` | target: members fit of members matched, from the last report in this session |
| `locked` / `drift` | target: a lock exists and is current / the lock disagrees with the directory |

Status comes from the files on disk at draw time (the sidecar's `[source]`
block; the blake3 is recomputed only when the mtime changed), so a command that
writes a sidecar changes the line with no extra plumbing.

**Console (bottom, default 8 rows).** Scrollback of `echo` + `text` per
command, input line last. `PgUp`/`PgDn` scroll; `Ctrl-L` toggles zoom (the
console takes the whole right column); `Ctrl-Up`/`Ctrl-Down` grow and shrink it
by a row; Up/Down at the input line recall history.

**Main pane (right, above the console).** Section 7.

**Focus.** One pane has focus, shown by its border. `Tab` cycles console →
browser → main → console; `Esc` anywhere returns focus to the console, so
typing is always one key away. The console has focus at startup. In the
browser: arrows move (and preview a file in the main pane, as a file manager
does), `Enter` opens a directory or sets a file as the context, `Backspace`
goes up; single letters **dispatch console lines** — `s` → `.sniff <selected>`,
`f` on a target → `.fit <it>`, `e` → `.edit <it>`, `d` toggles a mark and `D`
runs `.draft` over the marked files. The line appears in the scrollback as if
typed. There is no second code path.

**Status line.** Left: progress narration while a command runs (`Msg::Progress`)
or the last transient note (`Msg::Note`) — the distinction that exists today,
kept, because confusing them leaves the UI busy forever. Right: key hints for
the focused pane. The header carries the root path.

**Startup.** With no argument: root = current directory, context Empty. See
section 5 for the argument forms.

## 7. The main pane: one view per context

```rust
enum Context {
    Empty,
    File { path, sidecar: Option<Sidecar> },
    Pile { target, report: Option<PileReport> },
    Member { target, member },
    Evidence { target, member, rows },
    Query(QueryResult),
}
```

The context is set by the browser (`Enter`; arrow movement previews) and by the
last command's payload (`Sniffed` → File, `Fitted` → Pile, `Evidence` →
Evidence, `Query` → Query, `Shown` → File). `Esc` in the main pane steps back
one level: Evidence → Member → Pile.

- **Empty** — the root path, three lines of orientation, and (later) the mark.
- **File, no sidecar** — the *raw* file: the first screenful of lines as
  bytes-as-text, or the sheet grid for a workbook with a tab per sheet; then
  what is cheap to know without committing to anything — size, guessed format
  and encoding, whether the last row looks like a total. Footer: "not sniffed —
  `s`". This view must never look as though tdy has an opinion yet.
- **File, with sidecar** — two columns. Left: the same raw head, in the file's
  own header spelling (what a `matches` clause needs). Right: *what tdy makes
  of it* — the spec summary (extraction, transforms in order, each column as
  `name ← "source" : type`), then the **decisions list**: every sniff note that
  records a judgement (ambiguous date order, a thousands separator, a column
  widened to text, a dropped repeated header, a promoted multi-row header) with
  the two or three raw values that drove it. Confidence below the escalation
  threshold is red, beside the note that caused it. The preview table below. A
  stale sidecar shows the mismatch and offers `.sniff --force`.
- **Pile** — today's Pile screen: one line per member with status and reason,
  the counts, the target's columns and `matches` in a header block, lock drift
  from `lockfile::drift`. `Enter` on a member opens it.
- **Member** — today's Member screen: the gap next to the file's own header and
  raw rows; the remedy menu ranked by `--propose`. A remedy shows the textual
  diff of the target in an overlay and writes only on confirmation (today's
  `Confirm` screen becomes that overlay), then dispatches `.fit` again through
  the console, so the scrollback records the refit.
- **Evidence** — today's Accept screen, reached only from Member (section 8).
- **Query** — the result table with column types in the header, the row count,
  a truncation mark; when `.output` is routing, where the rows went instead.

What is "today's" above is `evidence.rs`, `remedy.rs` and the table renderers
in `ui.rs`, moved behind `Context`. The new work is the two File views and the
decisions list.

## 8. The review gate

The rules that hold today, restated for the console:

1. **Acceptance is reachable only through the evidence, one member at a
   time.** `.accept TARGET MEMBER` is a two-step command. The first run returns
   `Payload::Evidence` — raw values beside what they become, and the min and
   max over every row — and writes nothing; its text says "showing evidence for
   2025-07.csv; run `.accept` again to accept". Only the *same line* run again,
   with that evidence as the current context, performs the acceptance (what
   `fit --accept` does today). In the workbench, `a` on the Evidence view is
   that second line. Any other command in between resets to step one. No
   `.accept` takes more than one member; no flag skips step one.
2. **Every write to the target is preceded by a shown diff** — the remedy
   overlay. `.edit` is the honest exception: the user asked for their editor.
   On return the browser status updates and the console notes "target edited;
   lock is stale — `.fit` to re-prove".
3. **An acceptance is about bytes.** Carryover and expiry are unchanged
   (`Member { review, accepted }`, drift). The console adds and removes nothing
   here.

## 9. Errors and safety

- **Confinement.** The browser's root is the console's root. Every path in a
  dot-command, and every `messy()` / `dataset()` reference inside SQL, is
  confined at the point the file is opened, exactly as `mcp.rs` does it.
  Refusals say so.
- **A failed command is scrollback, not a modal.** The main pane keeps its
  context.
- **Overwrites.** `.draft --to` refuses an existing file. `.output` refuses an
  existing file without `--force`. `.sniff` keeps a fresh sidecar unless
  `--force` — today's behaviour, now said in the text output ("sidecar is
  fresh; `--force` to re-infer") so it stops being a surprise.
- **Worker panics.** The hook-and-flag pattern in today's `main.rs` carries
  over.
- **`.edit`.** Leave the alternate screen, run `$EDITOR`, re-enter, full
  redraw.

## 10. Tests

- **`tests/console.rs` (tdy).** `parse` line by line: every command, every
  flag, quoting, glob expansion against a tempdir, the multi-line `;` rule,
  every malformed line's message. Then `Session::run` end to end over a tempdir
  copy of `testdata/drifting_exports`: `.sniff` writes a sidecar and
  `Payload::Sniffed` agrees with `sidecar::load`; `.fit sales.tdy.sql` reports
  9/12 and writes no lock; `.accept` step one writes nothing and step two
  accepts; **the `text` of `.fit` equals what the `tdy fit` binary prints for
  the same pile** — the one-function promise, asserted; SQL results equal `tdy
  query`'s; a path outside the root is refused.
- **`tests/repl.rs` (tdy).** The binary with piped stdin: a script in, the
  expected text out, non-zero exit at the first error.
- **`tdy-tui/tests/render.rs`.** The existing `TestBackend` approach, extended:
  the three-pane frame at two terminal sizes; the browser's status column
  against a tempdir holding a sniffed, an unsniffed, a stale and a locked
  entry; each context's view; and the audit-trail property — a browser shortcut
  produces the same scrollback line as typing it.
- **`app.rs` stays a pure state machine** (`Key` in, `Action` out): focus
  cycling, `Esc` back-stepping and the two-step accept are unit tests with no
  terminal.

## 11. Slices

1. **Console.** `tdy::console` (`parse`, `Session`, `Outcome`), the plain
   console and batch runner behind `tdy` / `tdy console`, `tests/console.rs`,
   `tests/repl.rs`. README gets a "Console" section; the quick start switches
   to it. `tdy-tui` untouched.
2. **Frame.** `tdy-tui` rebuilt: browser, console pane, focus, status line,
   `Context::Empty` and the two File views; `tdy` with no args opens it when
   installed. The old screens still reachable behind a target argument so
   nothing regresses mid-slice.
3. **Views.** Pile, Member, Evidence and Query move behind `Context`; the
   remedy overlay; the two-step accept; the old screens deleted. The mark on
   the Empty and help views.

## 12. Decisions taken along the way

- **Dot-commands, not SQL extensions.** `SNIFF '2025-01.csv'` would parse, but
  the `.` marks the line as being about files and specs rather than about data
  — tdy's own split between sidecar and query. sqlite's users never confuse
  `.schema` with a query.
- **The grammar lives in the library, not the UI.** Otherwise the plain
  console and the workbench drift, and the CLI can never become a wrapper over
  the same dispatcher.
- **Text output is kept in the workbench even though the main pane shows the
  same facts.** The scrollback is the audit trail and the "same as the CLI"
  promise; duplication is the price and it is cheap.
- **The browser hides companions.** A sidecar is tdy's note about a file, not
  a second file to browse; showing it as status is what makes the tree read as
  the dataset rather than as a directory.
- **`tdy` alone opens the workbench when installed, the console otherwise.**
  The environment-dependent default is acceptable because it is announced once
  and both forms are one word away (`tdy ui`, `tdy console`).
- **No readline crate.** A minimal line editor keeps the dependency tree small
  for the published crate; history recall is the feature that matters.
