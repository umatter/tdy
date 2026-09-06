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

/// Refuse a compressed file being read as text, naming the fix.
///
/// Without this the bytes decode to mojibake and tdy reads them *confidently*
/// as a one-column table — not a wrong value exactly, since it faithfully
/// shows what the bytes are, but a confident answer to a question nobody
/// asked. A loud refusal naming `gunzip` is the honest reading of a file tdy
/// cannot open.
///
/// Deliberately magic-byte based rather than extension based: a `.csv` that is
/// really gzip is the case that produces the garbage, and it does not announce
/// itself in its name. Zip *is* listed even though every xlsx, xlsb and ods is
/// one: workbooks are routed to calamine by extension before any byte is read
/// as text, so a zip head reaching a text reader is a compressed export (or a
/// sidecar forcing a workbook through `delimited`), never a workbook.
///
/// Every magic here begins with a byte that cannot start UTF-8 text, except
/// bzip2's `BZh` — which is why that arm also demands the block-size digit
/// and the block magic that follow it, or `BZh_code,betrag` would be refused
/// as an archive.
///
/// This is enforced inside `read_all` and `read_head_tail`, so every text
/// reader in the tree is behind it; the streaming executor's raw opener
/// calls it on its own first buffer. Support, when it comes, is not a
/// decoder swap — see `docs/design/2026-09-06-compressed-inputs.md`.
pub fn refuse_if_compressed(path: &Path, head: &[u8]) -> Result<()> {
    let kind = match head {
        [0x1f, 0x8b, ..] => "gzip",
        [0x28, 0xb5, 0x2f, 0xfd, ..] => "zstd",
        // `BZh`, block size 1-9, then a block magic (pi) or, for an empty
        // stream, the end-of-stream magic (sqrt(pi)).
        [b'B', b'Z', b'h', b'1'..=b'9', 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, ..]
        | [b'B', b'Z', b'h', b'1'..=b'9', 0x17, 0x72, 0x45, 0x38, 0x50, 0x90, ..] => "bzip2",
        [0xfd, b'7', b'z', b'X', b'Z', ..] => "xz",
        [0x04, 0x22, 0x4d, 0x18, ..] => "lz4",
        [0x50, 0x4b, 0x03, 0x04, ..] => "zip",
        _ => return Ok(()),
    };
    anyhow::bail!(
        "{} is {kind}-compressed, and tdy reads it as text — which would give one column of \
         mojibake read confidently. Decompress it first (`gunzip`, `unzstd`, …); tdy does not \
         read compressed files yet",
        path.display()
    )
}

pub fn read_head_tail(path: &Path, head_bytes: usize, tail_bytes: usize) -> Result<HeadTail> {
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let total = f
        .metadata()
        .with_context(|| format!("cannot stat {}", path.display()))?
        .len();

    let head_len = head_bytes.min(usize::try_from(total).unwrap_or(usize::MAX));
    let mut head = vec![0u8; head_len];
    read_exact_or_eof(&mut f, &mut head)?;
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
    // Before the size check and before the read: a compressed file over the
    // limit should be told to decompress, not to raise the limit, and one
    // under it should be refused in constant memory, not after being read.
    {
        let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let mut head = vec![0u8; 16];
        read_exact_or_eof(&mut f, &mut head)?;
        refuse_if_compressed(path, &head)?;
    }
    if meta.len() > max_bytes {
        bail!(
            "{} is {:.1} GB, above the {:.1} GB limit for in-memory parsing \
             (raise [limits].max_file_bytes in the config if you really mean it)",
            path.display(),
            meta.len() as f64 / 1e9,
            max_bytes as f64 / 1e9
        );
    }
    std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))
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

    /// A gzip member of `region,betrag\nZH,10\n` (mtime 0), so the trailer
    /// is a real CRC32 and ISIZE — gunzip would accept it.
    const GZ: &[u8] = &[
        31, 139, 8, 0, 0, 0, 0, 0, 2, 3, 43, 74, 77, 207, 204, 207, 211, 73, 74, 45, 41, 74, 76,
        231, 138, 242, 208, 49, 52, 224, 2, 0, 129, 245, 9, 214, 20, 0, 0, 0,
    ];

    #[test]
    fn read_all_refuses_a_compressed_file_by_its_bytes() {
        let (_d, p) = tmpfile(GZ);
        let e = format!("{:#}", read_all(&p, u64::MAX).unwrap_err());
        assert!(e.contains("gzip-compressed"), "{e}");
    }

    #[test]
    fn read_head_tail_refuses_a_compressed_file_by_its_bytes() {
        let (_d, p) = tmpfile(GZ);
        let e = match read_head_tail(&p, 1024, 256) {
            Ok(_) => panic!("read gzip bytes as text"),
            Err(e) => format!("{e:#}"),
        };
        assert!(e.contains("gzip-compressed"), "{e}");
    }

    /// A file over the size limit is told to decompress, not to raise the
    /// limit — and is refused without being read into memory first.
    #[test]
    fn an_oversized_compressed_file_is_told_to_decompress_not_to_raise_the_limit() {
        let (_d, p) = tmpfile(GZ);
        let e = format!("{:#}", read_all(&p, 8).unwrap_err());
        assert!(e.contains("gzip-compressed"), "{e}");
        assert!(!e.contains("max_file_bytes"), "{e}");
    }

    /// Real bzip2 is `BZh`, a block-size digit, then the block magic. Text
    /// that merely starts with the letters `BZh` is text.
    #[test]
    fn text_beginning_with_the_letters_bzh_is_not_bzip2() {
        assert!(refuse_if_compressed(Path::new("x.csv"), b"BZh_code,betrag\nBZh1,10\n").is_ok());
        assert!(refuse_if_compressed(Path::new("x.csv"), b"BZh9 is a nice bus\n").is_ok());
    }

    #[test]
    fn a_real_bzip2_head_is_refused() {
        // `bz2.compress(b"region,betrag\nZH,10\n")[:12]`
        let head = [66u8, 90, 104, 57, 49, 65, 89, 38, 83, 89, 219, 58];
        let e = format!("{:#}", refuse_if_compressed(Path::new("x.csv"), &head).unwrap_err());
        assert!(e.contains("bzip2-compressed"), "{e}");
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
        let ht = read_head_tail(&p, 1024, 256).unwrap();
        assert_eq!(ht.head, b"hello world");
        assert!(ht.tail.is_none());
        assert_eq!(ht.total, 11);
        assert_eq!(ht.sampled, 11);
    }

    #[test]
    fn large_file_reads_only_the_ends() {
        let body: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let (_d, p) = tmpfile(&body);
        let ht = read_head_tail(&p, 1000, 100).unwrap();
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
        let ht = read_head_tail(&p, 1000, 400).unwrap();
        let tail = ht.tail.expect("a file longer than the head must expose its end");
        assert_eq!(tail.last(), body.last(), "the tail must reach the last byte");
    }

    #[test]
    fn head_and_tail_never_overlap() {
        let body: Vec<u8> = vec![b'x'; 1200];
        let (_d, p) = tmpfile(&body);
        let ht = read_head_tail(&p, 1000, 400).unwrap();
        assert_eq!(ht.head.len(), 1000);
        assert_eq!(ht.tail.map(|t| t.len()), Some(200), "tail must start where the head ended");
    }

    #[test]
    fn empty_file() {
        let (_d, p) = tmpfile(b"");
        let ht = read_head_tail(&p, 1024, 256).unwrap();
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
