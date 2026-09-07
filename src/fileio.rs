//! Bounded and streaming file access.
//!
//! Three things in tdy used to read an entire file when they needed a
//! fraction of it: building a 16 KB sample, fingerprinting, and writing the
//! sidecar. On a 2 GB export that is the difference between a tool that feels
//! instant and one that swaps.
//!
//! Everything here is deliberately allocation-bounded: no function in this
//! module allocates proportionally to the file size except [`read_all`],
//! which says so in its name.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Streaming chunk size. 256 KB is comfortably past the point where syscall
/// overhead matters and still nothing on a modern machine.
const CHUNK: usize = 256 * 1024;

/// A bounded look at a file: the first `head_bytes`, and — only if the file
/// is bigger than `head_bytes + tail_bytes` — its last `tail_bytes`.
pub struct HeadTail {
    pub head: Vec<u8>,
    pub tail: Option<Vec<u8>>,
    /// Full size on disk, from metadata (not from reading).
    pub total: u64,
    /// How many bytes were actually read.
    pub sampled: u64,
}

/// A compression tdy can read: the four whose decoders the tree already
/// carries. Detected by magic bytes, never by extension — a `.csv` that is
/// really gzip is the case that produced a confident column of mojibake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Gzip,
    Zstd,
    Bzip2,
    Xz,
}

impl Compression {
    /// The word the sidecar records.
    pub fn name(self) -> &'static str {
        match self {
            Compression::Gzip => "gzip",
            Compression::Zstd => "zstd",
            Compression::Bzip2 => "bzip2",
            Compression::Xz => "xz",
        }
    }
}

/// What the first bytes say the file is: a readable compression, an archive
/// tdy refuses (lz4, zip), or plain data.
///
/// Every magic here begins with a byte that cannot start UTF-8 text, except
/// bzip2's `BZh` — which is why that arm also demands the block-size digit
/// and the block magic that follow it, or `BZh_code,betrag` would be taken
/// for an archive. Zip *is* listed even though every xlsx, xlsb and ods is
/// one: workbooks are routed to calamine by extension before any byte is
/// read as text, so a zip head reaching a text reader is an archive.
enum Magic {
    Readable(Compression),
    Refused(&'static str),
    Plain,
}

fn magic(head: &[u8]) -> Magic {
    match head {
        [0x1f, 0x8b, ..] => Magic::Readable(Compression::Gzip),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Magic::Readable(Compression::Zstd),
        // `BZh`, block size 1-9, then a block magic (pi) or, for an empty
        // stream, the end-of-stream magic (sqrt(pi)).
        [b'B', b'Z', b'h', b'1'..=b'9', 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, ..]
        | [b'B', b'Z', b'h', b'1'..=b'9', 0x17, 0x72, 0x45, 0x38, 0x50, 0x90, ..] => {
            Magic::Readable(Compression::Bzip2)
        }
        [0xfd, b'7', b'z', b'X', b'Z', ..] => Magic::Readable(Compression::Xz),
        [0x04, 0x22, 0x4d, 0x18, ..] => Magic::Refused("lz4"),
        [0x50, 0x4b, 0x03, 0x04, ..] => Magic::Refused("zip"),
        _ => Magic::Plain,
    }
}

/// Which readable compression `head` announces, if any.
pub fn compression_of(head: &[u8]) -> Option<Compression> {
    match magic(head) {
        Magic::Readable(c) => Some(c),
        _ => None,
    }
}

/// Which readable compression `path` is, by its first bytes.
pub fn compression_kind(path: &Path) -> Result<Option<Compression>> {
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut head = vec![0u8; 16];
    read_exact_or_eof(&mut f, &mut head)?;
    Ok(compression_of(&head))
}

/// Refuse the archives tdy does not read, naming the ones it does.
pub fn refuse_if_compressed(path: &Path, head: &[u8]) -> Result<()> {
    if let Magic::Refused(kind) = magic(head) {
        anyhow::bail!(
            "{} is {kind}-compressed, which tdy does not read (it reads gzip, zstd, bzip2 and xz \
             compressed files, and a workbook by its own extension). Decompress it first",
            path.display()
        )
    }
    Ok(())
}

/// `5.0 kB`, `1.2 MB`, `4.3 GB` — a size the way a person reads it.
fn human_bytes(n: u64) -> String {
    let n = n as f64;
    if n >= 1e9 {
        format!("{:.1} GB", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1} MB", n / 1e6)
    } else {
        format!("{:.1} kB", n / 1e3)
    }
}

/// The process's cache of decompressed copies, under the system temp
/// directory and named by this process's id: created on first use, removed
/// by [`clear_cache`], which every binary calls on its way out. A crash
/// leaves it to the OS's temp cleanup — never gigabytes beside the user's
/// data.
fn cache_dir() -> Result<&'static Path> {
    static CACHE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let dir = CACHE.get_or_init(|| std::env::temp_dir().join(format!("tdy-{}", std::process::id())));
    std::fs::create_dir_all(dir).with_context(|| format!("creating the decompression cache {}", dir.display()))?;
    Ok(dir.as_path())
}

/// Remove this process's decompressed copies. Called by the binaries on
/// exit; harmless when nothing was ever materialised.
pub fn clear_cache() {
    let dir = std::env::temp_dir().join(format!("tdy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(dir);
}

/// The name a decompressed copy is read under: the file's own name with its
/// compression extension removed, so the format guess by extension still
/// works on the copy (`2025-01.csv.gz` reads as `2025-01.csv`). A `.csv`
/// that is really gzip keeps its name.
fn inner_name(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "data".into());
    let lower = name.to_ascii_lowercase();
    for ext in [".gz", ".gzip", ".zst", ".zstd", ".bz2", ".bzip2", ".xz", ".lzma"] {
        if lower.ends_with(ext) && name.len() > ext.len() {
            return name[..name.len() - ext.len()].to_string();
        }
    }
    name
}

/// A file as tdy reads it: the file itself, or — for a compressed one — a
/// decompressed copy in the process's cache, made once per file (keyed by
/// the compressed bytes' blake3) and bounded by `max_decompressed` before
/// it exists. Everything above this call sees a real file with byte
/// offsets: sampling, the streaming executor, xlguard, all unchanged.
pub fn materialize(path: &Path, max_decompressed: u64) -> Result<std::borrow::Cow<'_, Path>> {
    use std::borrow::Cow;
    use std::io::{BufReader, Read as _};
    let Some(kind) = compression_kind(path)? else { return Ok(Cow::Borrowed(path)) };
    let (hash, _) = hash_file(path)?;
    let dir = cache_dir()?.join(hash.trim_start_matches("b3:"));
    let copy = dir.join(inner_name(path));
    if copy.is_file() {
        return Ok(Cow::Owned(copy));
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let f = BufReader::with_capacity(CHUNK, File::open(path).with_context(|| format!("cannot open {}", path.display()))?);
    let mut reader: Box<dyn Read> = match kind {
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(f)),
        Compression::Zstd => Box::new(zstd::Decoder::new(f).context("opening zstd stream")?),
        Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(f)),
        Compression::Xz => Box::new(xz2::read::XzDecoder::new_multi_decoder(f)),
    };
    // Write to a sibling and rename, so a copy that is present is complete —
    // a second process, or a second thread, must never read a half-written
    // one — and a copy that crosses the ceiling is removed, not left. The
    // sibling's name is unique to this writer: two threads materialising
    // the same file race to the rename, and the loser must find the copy
    // in place rather than its own part gone.
    static WRITER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = WRITER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{}.{}.part", inner_name(path), n));
    let result = (|| -> Result<()> {
        let mut out = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let mut buf = vec![0u8; CHUNK];
        let mut written: u64 = 0;
        loop {
            let n = reader.read(&mut buf).with_context(|| format!("decompressing {}", path.display()))?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > max_decompressed {
                bail!(
                    "{} decompresses to more than {}, above [limits].max_decompressed_bytes \
                     ({}); raise it in the config if you really mean it",
                    path.display(),
                    human_bytes(written),
                    human_bytes(max_decompressed)
                );
            }
            out.write_all(&buf[..n]).with_context(|| format!("writing {}", tmp.display()))?;
        }
        out.sync_all().ok();
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // Another writer may have placed the copy meanwhile; theirs is as good
    // as ours (same bytes, same key), so keep it rather than replace it.
    if copy.is_file() {
        let _ = std::fs::remove_file(&tmp);
    } else if let Err(e) = std::fs::rename(&tmp, &copy) {
        let _ = std::fs::remove_file(&tmp);
        if !copy.is_file() {
            return Err(e).with_context(|| format!("placing {}", copy.display()));
        }
    }
    Ok(Cow::Owned(copy))
}

pub fn read_head_tail(path: &Path, head_bytes: usize, tail_bytes: usize, max_decompressed: u64) -> Result<HeadTail> {
    let path = materialize(path, max_decompressed)?;
    let path = path.as_ref();
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let total = f
        .metadata()
        .with_context(|| format!("cannot stat {}", path.display()))?
        .len();

    let head_len = head_bytes.min(usize::try_from(total).unwrap_or(usize::MAX));
    let mut head = vec![0u8; head_len];
    read_exact_or_eof(&mut f, &mut head)?;
    // A readable compression was materialised above and this is its copy;
    // an archive tdy does not read (lz4, zip) is still the original, and is
    // refused here by name rather than read as one column of mojibake.
    refuse_if_compressed(path, &head)?;
    let mut sampled = head.len() as u64;

    // Any file bigger than the head has an unseen end. Reading the tail only
    // when the file exceeds head+tail leaves a band of sizes where the last
    // line is never looked at — and the last line is where a "Total" row is.
    let tail = if total > head_len as u64 && tail_bytes > 0 {
        let start = (total - tail_bytes.min(usize::try_from(total).unwrap_or(usize::MAX)) as u64)
            .max(head_len as u64);
        f.seek(SeekFrom::Start(start))
            .with_context(|| format!("cannot seek in {}", path.display()))?;
        let want = usize::try_from(total - start).unwrap_or(tail_bytes).min(tail_bytes);
        let mut buf = vec![0u8; want];
        read_exact_or_eof(&mut f, &mut buf)?;
        sampled += buf.len() as u64;
        (!buf.is_empty()).then_some(buf)
    } else {
        None
    };

    Ok(HeadTail { head, tail, total, sampled })
}

/// Fill `buf` as far as the file allows, truncating it to what was read.
fn read_exact_or_eof(f: &mut File, buf: &mut Vec<u8>) -> Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = f.read(&mut buf[filled..]).context("read failed")?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(())
}

/// Read a whole file, refusing anything above `max_bytes` with an actionable
/// message instead of an out-of-memory kill.
pub fn read_all(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("cannot stat {}", path.display()))?;
    if meta.is_dir() {
        bail!("{} is a directory, not a data file", path.display());
    }
    // Before the size check and before the read: an archive tdy does not
    // read is refused by name in constant memory, and one it does read is
    // materialised — bounded by the same ceiling an in-memory read has —
    // so the size checked and the bytes read are the copy's.
    {
        let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let mut head = vec![0u8; 16];
        read_exact_or_eof(&mut f, &mut head)?;
        refuse_if_compressed(path, &head)?;
    }
    let real = materialize(path, max_bytes)?;
    let real: &Path = real.as_ref();
    let meta = std::fs::metadata(real)
        .with_context(|| format!("cannot stat {}", real.display()))?;
    if meta.len() > max_bytes {
        bail!(
            "{} is {:.1} GB, above the {:.1} GB limit for in-memory parsing \
             (raise [limits].max_file_bytes in the config if you really mean it)",
            path.display(),
            meta.len() as f64 / 1e9,
            max_bytes as f64 / 1e9
        );
    }
    std::fs::read(real).with_context(|| format!("cannot read {}", real.display()))
}

/// blake3 of a file's contents, streamed through a fixed buffer.
pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    let f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut r = BufReader::with_capacity(CHUNK, f);
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = r.read(&mut buf).with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hasher.finalize().to_hex().to_string(), total))
}

/// Resolve `path` and prove it lives under `root`, or say why not.
///
/// The one confinement check for everything the MCP server touches: tool
/// arguments, the file references inside SQL, the members a target's globs
/// resolve to, and the paths recorded in a lock. Canonicalisation resolves
/// `../` and symlinks, so a link inside the root pointing outside it is
/// refused; `starts_with` compares whole components, so `/root-evil` is not
/// inside `/root`. Callers must then *use the returned path*, not the raw
/// one — resolving once and opening through the same resolved path is what
/// closes the gap between the check and the open.
///
/// `root` must itself be canonical (the server canonicalises it at startup).
pub fn confine(path: &Path, root: &Path) -> Result<PathBuf> {
    confine_from(path, root, root)
}

/// [`confine`], with a separate directory for a *relative* path to join
/// onto.
///
/// The two are the same thing for the MCP server, whose working directory
/// *is* its root, and different for the console, where `.cd` moves a working
/// directory that stays inside the root. A relative `messy('x.csv')` there
/// means the `x.csv` the user would see in `.ls` — joining it onto the root
/// instead would silently read a different file of the same name, which is
/// the one thing this project refuses to do. `base` is only the join point;
/// `root` is still the whole of what is allowed, so a `base` inside the root
/// cannot widen it.
pub fn confine_from(path: &Path, base: &Path, root: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let canon = joined.canonicalize().with_context(|| {
        format!("{} does not exist under {}", path.display(), root.display())
    })?;
    if !canon.starts_with(root) {
        bail!(
            "{} is outside this server's --root ({})",
            path.display(),
            root.display()
        );
    }
    Ok(canon)
}

/// Write a file so that it is either the old contents or the new ones, never
/// a half-written mixture: write a sibling temp file, then rename over the
/// target. A sidecar is a record of provenance; a truncated one is worse than
/// none, because the next run would trust its header.
pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tdy".to_string());
    // Unique per process: two tdy runs stamping the same sidecar (a parallel
    // CI matrix, say) would otherwise write the same temp file and rename each
    // other's half-written bytes into place.
    let tmp: PathBuf = dir.join(format!(".{name}.{}.tmp", std::process::id()));

    {
        let mut f = File::create(&tmp).with_context(|| {
            format!(
                "cannot create {} (is {} writable?)",
                tmp.display(),
                dir.display()
            )
        })?;
        f.write_all(contents.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all().with_context(|| format!("flushing {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot replace {}", path.display())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(body: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("f.bin");
        let mut f = File::create(&p).unwrap();
        f.write_all(body).unwrap();
        (d, p)
    }

    fn gz_of(text: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(text).unwrap();
        e.finish().unwrap()
    }

    /// A compressed file is materialised once — into the process's cache
    /// under its inner name — and read as its contents. A second touch finds
    /// the copy rather than decompressing again.
    #[test]
    fn a_gzip_file_is_materialised_once_and_read_as_its_contents() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("x.csv.gz");
        std::fs::write(&p, gz_of(b"region,betrag\nZH,10\nBE,11\n")).unwrap();
        let m1 = materialize(&p, u64::MAX).unwrap().into_owned();
        assert!(m1.ends_with("x.csv"), "the inner name, so the format guess still works: {}", m1.display());
        assert_ne!(m1, p);
        assert_eq!(std::fs::read(&m1).unwrap(), b"region,betrag\nZH,10\nBE,11\n");
        let stamp = std::fs::metadata(&m1).unwrap().modified().unwrap();
        let m2 = materialize(&p, u64::MAX).unwrap().into_owned();
        assert_eq!(m1, m2, "the same file materialises to the same copy");
        assert_eq!(std::fs::metadata(&m2).unwrap().modified().unwrap(), stamp, "and was not rewritten");
        // A plain file is handed back as itself.
        let plain = d.path().join("y.csv");
        std::fs::write(&plain, b"a,b\n").unwrap();
        assert_eq!(materialize(&plain, u64::MAX).unwrap().as_ref(), plain.as_path());
    }

    #[test]
    fn the_readers_see_the_decompressed_bytes() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("x.csv.gz");
        std::fs::write(&p, gz_of(b"region,betrag\nZH,10\n")).unwrap();
        assert_eq!(read_all(&p, u64::MAX).unwrap(), b"region,betrag\nZH,10\n");
        let ht = read_head_tail(&p, 6, 4, u64::MAX).unwrap();
        assert_eq!(ht.head, b"region");
        assert_eq!(ht.tail.as_deref(), Some(&b",10\n"[..]));
        assert_eq!(ht.total, 20, "the decompressed size, which is what sampling reasons about");
    }

    /// Every format the tree already knows how to decode, and the two it
    /// still refuses — naming what it does read.
    #[test]
    fn zstd_bzip2_and_xz_materialise_and_lz4_and_zip_are_refused_by_name() {
        use std::io::Write as _;
        let d = tempfile::TempDir::new().unwrap();
        let text = b"a,b\n1,2\n";
        let zst = zstd::encode_all(&text[..], 3).unwrap();
        let mut bz = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        bz.write_all(text).unwrap();
        let bz = bz.finish().unwrap();
        let mut xz = xz2::write::XzEncoder::new(Vec::new(), 6);
        xz.write_all(text).unwrap();
        let xz = xz.finish().unwrap();
        for (name, bytes) in [("x.csv.zst", zst), ("x.csv.bz2", bz), ("x.csv.xz", xz)] {
            let p = d.path().join(name);
            std::fs::write(&p, bytes).unwrap();
            assert_eq!(read_all(&p, u64::MAX).unwrap(), text, "{name}");
            assert!(materialize(&p, u64::MAX).unwrap().ends_with("x.csv"), "{name}");
        }
        for (name, head) in [("x.csv.lz4", &[0x04u8, 0x22, 0x4d, 0x18, 0, 0][..]), ("x.csv.zip", b"PK\x03\x04\x14\x00")] {
            let p = d.path().join(name);
            std::fs::write(&p, head).unwrap();
            let e = format!("{:#}", read_all(&p, u64::MAX).unwrap_err());
            assert!(e.contains("gzip, zstd, bzip2 and xz"), "{name}: {e}");
            assert!(e.to_lowercase().contains("decompress"), "{name}: {e}");
        }
    }

    /// The decompressed size is bounded before it exists: a copy that would
    /// cross the ceiling is refused naming the setting, and no partial copy
    /// is left behind.
    #[test]
    fn a_decompressed_file_over_the_ceiling_is_refused_and_leaves_nothing() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("big.csv.gz");
        let text: Vec<u8> = std::iter::repeat_n(b"0123456789\n", 1000).flatten().copied().collect();
        std::fs::write(&p, gz_of(&text)).unwrap();
        let e = format!("{:#}", materialize(&p, 5_000).unwrap_err());
        assert!(e.contains("max_decompressed_bytes"), "{e}");
        assert!(e.contains("5"), "the ceiling is named: {e}");
        let partial = materialize(&p, u64::MAX).unwrap().into_owned();
        assert_eq!(std::fs::read(&partial).unwrap().len(), text.len(), "a later, allowed read gets the whole file");
    }

    /// Real bzip2 is `BZh`, a block-size digit, then the block magic. Text
    /// that merely starts with the letters `BZh` is text.
    #[test]
    fn text_beginning_with_the_letters_bzh_is_not_bzip2() {
        assert!(refuse_if_compressed(Path::new("x.csv"), b"BZh_code,betrag\nBZh1,10\n").is_ok());
        assert!(refuse_if_compressed(Path::new("x.csv"), b"BZh9 is a nice bus\n").is_ok());
    }

    #[test]
    fn a_real_bzip2_head_is_recognised_as_bzip2() {
        // `bz2.compress(b"region,betrag\nZH,10\n")[:12]`
        let head = [66u8, 90, 104, 57, 49, 65, 89, 38, 83, 89, 219, 58];
        assert_eq!(compression_of(&head), Some(Compression::Bzip2));
        assert!(refuse_if_compressed(Path::new("x.csv"), &head).is_ok(), "readable, so not refused");
        assert_eq!(compression_of(b"BZh9 is a nice bus\n"), None);
    }

    /// Nothing that reaches a text reader can be a workbook — those are
    /// routed to calamine by extension before any byte is read — so a zip
    /// head here is a compressed export, never an xlsx.
    #[test]
    fn a_zip_is_refused_like_every_other_archive() {
        let e = format!(
            "{:#}",
            refuse_if_compressed(Path::new("sales.csv.zip"), b"PK\x03\x04\x14\x00").unwrap_err()
        );
        assert!(e.contains("zip-compressed"), "{e}");
    }

    #[test]
    fn small_file_is_all_head_no_tail() {
        let (_d, p) = tmpfile(b"hello world");
        let ht = read_head_tail(&p, 1024, 256, u64::MAX).unwrap();
        assert_eq!(ht.head, b"hello world");
        assert!(ht.tail.is_none());
        assert_eq!(ht.total, 11);
        assert_eq!(ht.sampled, 11);
    }

    #[test]
    fn large_file_reads_only_the_ends() {
        let body: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let (_d, p) = tmpfile(&body);
        let ht = read_head_tail(&p, 1000, 100, u64::MAX).unwrap();
        assert_eq!(ht.head.len(), 1000);
        assert_eq!(ht.head[..], body[..1000]);
        let tail = ht.tail.unwrap();
        assert_eq!(tail.len(), 100);
        assert_eq!(tail[..], body[body.len() - 100..]);
        assert_eq!(ht.total, 100_000);
        assert_eq!(ht.sampled, 1100, "must not read the middle of the file");
    }

    #[test]
    fn a_file_between_head_and_head_plus_tail_still_has_a_tail() {
        // The size band where the end of the file used to be invisible.
        let body: Vec<u8> = (0..1500u32).map(|i| b'a' + (i % 26) as u8).collect();
        let (_d, p) = tmpfile(&body);
        let ht = read_head_tail(&p, 1000, 400, u64::MAX).unwrap();
        let tail = ht.tail.expect("a file longer than the head must expose its end");
        assert_eq!(tail.last(), body.last(), "the tail must reach the last byte");
    }

    #[test]
    fn head_and_tail_never_overlap() {
        let body: Vec<u8> = vec![b'x'; 1200];
        let (_d, p) = tmpfile(&body);
        let ht = read_head_tail(&p, 1000, 400, u64::MAX).unwrap();
        assert_eq!(ht.head.len(), 1000);
        assert_eq!(ht.tail.map(|t| t.len()), Some(200), "tail must start where the head ended");
    }

    #[test]
    fn empty_file() {
        let (_d, p) = tmpfile(b"");
        let ht = read_head_tail(&p, 1024, 256, u64::MAX).unwrap();
        assert!(ht.head.is_empty());
        assert!(ht.tail.is_none());
        assert_eq!(ht.total, 0);
    }

    #[test]
    fn hashing_matches_the_one_shot_hash() {
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        let (_d, p) = tmpfile(&body);
        let (h, n) = hash_file(&p).unwrap();
        assert_eq!(n, body.len() as u64);
        assert_eq!(h, blake3::hash(&body).to_hex().to_string());
    }

    #[test]
    fn read_all_refuses_oversized_files() {
        let (_d, p) = tmpfile(&vec![0u8; 4096]);
        assert!(read_all(&p, 1024).is_err());
        assert!(read_all(&p, 8192).is_ok());
    }

    #[test]
    fn read_all_rejects_a_directory_with_a_clear_message() {
        let d = tempfile::TempDir::new().unwrap();
        let err = read_all(d.path(), u64::MAX).unwrap_err();
        assert!(format!("{err:#}").contains("directory"));
    }

    #[test]
    fn confine_resolves_and_refuses_escapes() {
        let root_dir = tempfile::TempDir::new().unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(root.join("in.csv"), "a\n1\n").unwrap();
        std::fs::write(outside.path().join("secret.csv"), "a\n1\n").unwrap();

        // A relative path inside the root resolves to its canonical form.
        let ok = confine(Path::new("in.csv"), &root).unwrap();
        assert_eq!(ok, root.join("in.csv"));

        // `../` must not escape, and neither must an absolute path outside.
        assert!(confine(Path::new("../secret.csv"), &root).is_err());
        let abs = outside.path().join("secret.csv");
        let err = confine(&abs, &root).unwrap_err();
        assert!(format!("{err:#}").contains("outside"), "{err:#}");

        // A symlink inside the root pointing outside it is an escape: the
        // check is on the resolved target, not on where the link sits.
        #[cfg(unix)]
        {
            let link = root.join("link.csv");
            std::os::unix::fs::symlink(outside.path().join("secret.csv"), &link).unwrap();
            let err = confine(Path::new("link.csv"), &root).unwrap_err();
            assert!(format!("{err:#}").contains("outside"), "{err:#}");
        }

        // A sibling directory sharing the root's name as a prefix is not
        // inside it: the comparison is per component, not per byte.
        let sibling = root
            .parent()
            .unwrap()
            .join(format!("{}-evil", root.file_name().unwrap().to_string_lossy()));
        std::fs::create_dir(&sibling).unwrap();
        std::fs::write(sibling.join("x.csv"), "a\n").unwrap();
        assert!(confine(&sibling.join("x.csv"), &root).is_err());
        std::fs::remove_dir_all(&sibling).unwrap();
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_file() {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("sidecar.toml");
        atomic_write(&p, "first").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "first");
        atomic_write(&p, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "second");
        let leftovers: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
    }

    #[test]
    fn atomic_write_into_an_unwritable_directory_says_where() {
        let p = Path::new("/proc/definitely/not/writable/x.toml");
        let err = atomic_write(p, "x").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not/writable") || msg.contains("cannot create"), "{msg}");
    }
}
