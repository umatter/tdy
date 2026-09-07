# Compressed inputs

*2026-09-06. Why `.csv.gz` is not the afternoon's work the shape slice called
it, and what the actual decision is. Option C below is what landed (§4 records
where); this page exists because the estimate was wrong and the reason is
worth writing down.*

---

## 1. The need is ordinary

Monthly exports arrive zipped. `dataset()` exists to read a pile of monthly
exports. Today a `.csv.gz` is refused and a zip of CSVs cannot be a member,
so the first thing anyone does with a real export directory is unpack it — which
means the pile tdy locks is a *copy* of the pile they were given, and the
fingerprints are of files nobody else has.

That last part is the sharp end. The lock's whole claim is "these bytes, this
answer". If the bytes are a local decompression, the claim is about a derived
artifact.

## 2. Why the shape slice's plan was wrong

`docs/design/2026-09-06-shape-slice.md` §S6 said:

> `open_input` gains a gzip branch keyed on the extension, checked against the
> magic bytes. It streams (`flate2`'s reader is a `BufRead`), so nothing about
> the memory story changes.

Every clause of that is true, and it describes only the **executor**. The
executor is not the problem. These are:

- **`sample::build` reads a head *and a tail*, by byte offset.** A compressed
  stream has no byte offsets. Reaching the last 8 KB of a `.gz` means
  decompressing all of it — precisely the bound the sampler exists to keep, and
  the reason a 2 GB file sniffs as fast as a 2 MB one.
- **`fileio::read_head_tail` seeks.** Same problem, one layer down.
- **`xlguard::preflight` measures before reading**, because a spreadsheet's
  declared size is a claim. A compressed file's size is a claim twice over, and
  the existing zip-expansion check covers the container, not a member's own
  compression.
- **`sidecar` fingerprints the file.** Which file — the bytes on disk, or the
  bytes they decompress to? Both are defensible and they are different answers.

So the question is not "which decoder", it is **what is a sample of a compressed
file**, and there is no answer that is obviously right.

## 3. The options

### A · Head-only sampling, `partial = true` always

Decompress the first N KB, sniff from that, mark the sample partial.

- **Cheap.** One branch in `sample::build`, no new storage, memory unchanged.
- **Costs every tail-based inference.** `footer_rows`, `trailing_prose_block`
  and `note_interior_summary` all read the *end* of the table; a trailing
  `Total` row would stop being detectable. That is one of the things sniffing
  is for, and the corpus audit found interior and trailing totals in 11 of 320
  files.
- Already-handled downstream: `SkipRows{tail}` is skipped on a truncated table,
  so nothing would silently mis-skip. It would simply see less.

### B · Decompress to a temp file on open

The first thing that touches the path materialises it, and everything after
that is ordinary.

- **Correct everywhere.** Sampling, xlguard, streaming, all unchanged.
- **Costs disk, not memory** — and a decompression bomb costs a lot of it, so
  it needs its own ceiling (`[limits] max_decompressed_bytes`), which is a new
  concept in a place that currently has one.
- **Needs a lifetime.** Who deletes it, and when? A query naming the same file
  twice must not decompress twice; a crash must not leave gigabytes behind.
  That is a cache with an eviction policy, which tdy does not otherwise have.
- **The fingerprint question becomes visible**, which is good: hash the
  compressed bytes, because those are the bytes the user has and the ones that
  arrive again next month.

### C · Refuse clearly, and say so

Keep it unreadable, but make the error name the fix (`gunzip`) rather than
what happened before: not an encoding error but a *success* — `decode_reporting`
substitutes U+FFFD and never fails, so a gzip member read as text was one
column of mojibake with a confidence score attached. Not a wrong value in the
strict sense, since those really are the bytes, but a confident answer to a
question nobody asked, which is the same failure in different clothes.

- **Honest and free.** No new concept, no bomb ceiling, no cache.
- Leaves the pile-of-zipped-exports case exactly where it is.

## 4. Recommendation

**C now, B when someone actually has the pile.** *B landed on 2026-09-07:*
`fileio::materialize` decompresses gzip, zstd, bzip2 and xz (the decoders the
tree already carried) into a process-lifetime cache keyed by the compressed
bytes' blake3, bounded by `[limits].max_decompressed_bytes` before the copy
exists; every byte reader and the one workbook opener go through it, the
sidecar fingerprints the compressed bytes and records the format. lz4 and zip
stay refused. The cache is cleared at each binary's exit; a crash leaves it to
the OS. What follows is the reasoning as it stood before that.

C landed with this page. `fileio::refuse_if_compressed` matches gzip, zstd,
bzip2 (magic *and* block header, since `BZh` alone is printable text), xz, lz4
and zip — by magic bytes, not extension, because the `.csv` that is really
gzip is the case that produced the garbage. Zip is included on purpose:
workbooks are routed to calamine by extension before any byte is read as
text, so a zip head reaching a text reader is a compressed export, never an
xlsx. The check lives *inside* `fileio::read_all` and `fileio::read_head_tail`
(before the size limit and before the read, so an oversized archive is told
to decompress rather than to raise `max_file_bytes`, in constant memory), and
the streaming executor's raw opener — which reads bytes directly when a
sidecar declares `utf-8` and never touches those readers — calls it on its
first buffer. The first cut put it at three call sites and missed that
opener; `tests/streaming.rs` now pins that both executors refuse the file.

C is a message change and can ride with anything. B is the right long-term
answer — A trades away detection that this project's own audits keep finding
matters, and a "partial sample" that is *always* partial makes the confidence
score mean something different for compressed files than for plain ones, which
is worse than not supporting them.

B should not be built speculatively: it introduces a temp-file cache and a
decompression ceiling, and neither should exist until a real directory needs
them. When it does, the order is: ceiling first (a bomb must be refused before
anything is written), then materialisation, then the cache.

## 5. What B would touch

| | |
|---|---|
| `fileio` | `open_input` materialises; the ceiling lives here |
| `sample.rs` | unchanged, which is the point |
| `sidecar` | hash the compressed bytes; record that it did |
| `config::Limits` | `max_decompressed_bytes` |
| `provider`, `dataset` | a temp-file lifetime spanning one query |
| `xlguard` | unchanged; it sees a real file |

## 6. Out of scope on purpose

A **zip of several CSVs** is a different question and belongs with
`docs/design/2026-09-06-members-and-regions.md`: it is not a compressed file, it
is a *container of members*, and the thing to decide there is what a member is.
