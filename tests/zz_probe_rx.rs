#[test]
fn probe_regex_size_gap() {
    let pat = "(?:a{100}){100}{100}";
    for pat in ["(?:a{1000}){1000}", "(?:\\w{500}){500}", "[\\p{L}\\p{N}]{1000}{1000}"] {
        let std_ok = regex::Regex::new(pat).is_ok();
        let lim_ok = regex::RegexBuilder::new(pat).size_limit(8*1024*1024).dfa_size_limit(8*1024*1024).build().is_ok();
        eprintln!("{pat:?}: default={std_ok} limited={lim_ok}");
    }
    let _ = pat;
}
