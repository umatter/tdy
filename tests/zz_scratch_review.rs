use std::path::{Path, PathBuf};
use tdy::config::Limits;
use tdy::{engine, sample, sniff, stream};
fn testdata() -> PathBuf { Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata") }
fn fixtures() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() { let p = e.path();
            if p.is_dir() { if p.file_name().map(|n| n=="large"||n=="gen").unwrap_or(false) {continue}; walk(&p,out); }
            else { let ext=p.extension().map(|e|e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
                if matches!(ext.as_str(),"csv"|"tsv"|"txt"|"dat"|"log"|"ndjson"|"jsonl"|"json"|"xlsx"|"xlsm"|"xls"|"ods"){out.push(p);} } } }
    let mut v=Vec::new(); walk(&testdata(),&mut v); v.sort(); v }
#[test]
fn where_do_they_drop_out() {
    for p in fixtures() {
        let n = p.file_name().unwrap().to_string_lossy().to_string();
        let Ok(s) = sample::build(&p, 16*1024, Limits::default()) else { eprintln!("SKIP sample  {n}"); continue };
        let Ok(res) = sniff::sniff(&p, &s, Limits::default()) else { eprintln!("SKIP sniff   {n}"); continue };
        let spec = res.spec;
        if let Err(e) = spec.validate() { eprintln!("SKIP validate {n}: {e:?}"); continue }
        if engine::schema_of(&spec).is_err() { eprintln!("SKIP schema  {n}"); continue }
        match engine::execute_batches(&spec, &p, Limits::default()) {
            Err(e) => eprintln!("SKIP execute {n}: {}", format!("{e:#}").lines().next().unwrap_or("")),
            Ok(b) => { if b.is_empty() { eprintln!("EMPTY BATCHES {n}"); } }
        }
        if !stream::can_stream(&spec) { eprintln!("NO-STREAM    {n}"); }
    }
}
