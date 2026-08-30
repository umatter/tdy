use tdy::target::Target;
#[test]
fn probe_table_constraints() {
    for sql in [
        "CREATE TABLE s (a TEXT, b TEXT, PRIMARY KEY (a)) WITH (files='x')",
        "CREATE TABLE s (a TEXT, UNIQUE (a)) WITH (files='x')",
        "CREATE TABLE s (a TEXT, CHECK (a <> '')) WITH (files='x')",
        "CREATE TABLE s (a TEXT, FOREIGN KEY (a) REFERENCES t(b)) WITH (files='x')",
        "CREATE TEMPORARY TABLE s (a TEXT) WITH (files='x')",
        "CREATE TABLE IF NOT EXISTS s (a TEXT) WITH (files='x')",
        "CREATE TABLE s (a TEXT COLLATE de_CH) WITH (files='x')",
        "CREATE TABLE s (a TEXT DEFAULT 'x') WITH (files='x')",
        "CREATE TABLE s (a INT) WITH (files='x')",
        "CREATE TABLE s (a SMALLINT) WITH (files='x')",
        "CREATE TABLE s (a REAL) WITH (files='x')",
        "CREATE TABLE s (a TEXT) AS SELECT 1",
    ] {
        match Target::parse(sql) {
            Ok(t) => println!("OK  {sql}\n      -> {:?}", t.columns.iter().map(|c| (c.name.clone(), format!("{:?}", c.dtype), c.nullable)).collect::<Vec<_>>()),
            Err(e) => println!("ERR {sql}\n      -> {}", format!("{e:#}").lines().next().unwrap_or("")),
        }
    }
}
