//! A dataset member: a file, or one sheet of a workbook.
//!
//! Two fields, never one string: a `#` in a file name or a sheet name must
//! not make a lock ambiguous. The textual `path#sheet` exists where a person
//! reads or types a member, and a typed reference is resolved against the
//! members that exist rather than split by a rule.

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemberRef {
    /// Relative to the target's directory, as lock members are.
    pub path: String,
    /// The sheet of a workbook this member is, by its name in the workbook.
    /// `None` is the whole file — every other format, and a workbook only
    /// one of whose sheets produces the declared table.
    pub sheet: Option<String>,
    /// One of several tables stacked in the file or sheet, counted from 1
    /// in file order. `None` is the whole file or sheet.
    pub region: Option<u32>,
}

impl MemberRef {
    pub fn file(path: impl Into<String>) -> MemberRef {
        MemberRef { path: path.into(), sheet: None, region: None }
    }

    pub fn sheet(path: impl Into<String>, sheet: impl Into<String>) -> MemberRef {
        MemberRef { path: path.into(), sheet: Some(sheet.into()), region: None }
    }

    pub fn region(path: impl Into<String>, sheet: Option<String>, region: u32) -> MemberRef {
        MemberRef { path: path.into(), sheet, region: Some(region) }
    }

    /// The form a person reads and types: `path`, or `path#sheet`, or `path#region`, or `path#sheet#region`.
    pub fn name(&self) -> String {
        let mut s = self.path.clone();
        if let Some(sh) = &self.sheet { s.push('#'); s.push_str(sh); }
        if let Some(r) = self.region { s.push('#'); s.push_str(&r.to_string()); }
        s
    }

    /// Resolve a typed reference against the members that exist. Every
    /// split at a `#` is a candidate — the whole text as a plain member,
    /// then each `path#sheet` split from the right, and also region
    /// candidates if the rightmost segment is a positive integer.
    ///
    /// It does not stop at the first candidate that `exists`: it collects
    /// *all* of them, so `Ok(Some(m))` means exactly one reading was true.
    /// `Ok(None)` names no member, and `Err` carries every member the text
    /// could mean when more than one was — taking the first would be
    /// picking one of two answers in silence. `exists` therefore has to be
    /// exact: a predicate that says yes to two readings of one member makes
    /// that member unnameable (see `sidecar::declares_member`).
    pub fn resolve(text: &str, exists: impl Fn(&MemberRef) -> bool) -> Result<Option<MemberRef>, Vec<MemberRef>> {
        let mut found: Vec<MemberRef> = Vec::new();
        let mut consider = |m: MemberRef| { if exists(&m) && !found.contains(&m) { found.push(m); } };
        consider(MemberRef::file(text));
        for (i, _) in text.rmatch_indices('#') {
            let (head, tail) = (&text[..i], &text[i + 1..]);
            consider(MemberRef::sheet(head, tail));
            if let Ok(n) = tail.parse::<u32>() {
                if n >= 1 {
                    consider(MemberRef::region(head, None, n));
                    for (j, _) in head.rmatch_indices('#') {
                        consider(MemberRef::region(&head[..j], Some(head[j + 1..].to_string()), n));
                    }
                }
            }
        }
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            // Two members could be meant. Taking one silently is the
            // wrong-value failure this tool refuses; the caller names both.
            _ => Err(found),
        }
    }

    /// The names of several candidates, for an error message.
    pub fn names(members: &[MemberRef]) -> String {
        members.iter().map(|m| format!("{:?}", m.name())).collect::<Vec<_>>().join(" and ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_member_names_its_path_and_a_sheet_member_appends_the_sheet() {
        assert_eq!(MemberRef::file("2025.xlsx").name(), "2025.xlsx");
        assert_eq!(MemberRef::sheet("2025.xlsx", "Q1").name(), "2025.xlsx#Q1");
    }

    /// `a#b#c` could be file `a#b` sheet `c`, or file `a` sheet `b#c`, or a
    /// file called `a#b#c`. Whichever exists is the answer; nothing splits
    /// the string by rule.
    #[test]
    fn a_typed_reference_resolves_against_what_exists() {
        let members = [
            MemberRef::sheet("2025#final.xlsx", "Q1"),
            MemberRef::sheet("2025.xlsx", "Q#2"),
            MemberRef::file("plain#name.csv"),
        ];
        let exists = |m: &MemberRef| members.contains(m);
        assert_eq!(MemberRef::resolve("2025#final.xlsx#Q1", exists), Ok(Some(members[0].clone())));
        assert_eq!(MemberRef::resolve("2025.xlsx#Q#2", exists), Ok(Some(members[1].clone())));
        assert_eq!(MemberRef::resolve("plain#name.csv", exists), Ok(Some(members[2].clone())));
        assert_eq!(MemberRef::resolve("2025.xlsx#Q3", exists), Ok(None));
        assert_eq!(MemberRef::resolve("nothing.csv", exists), Ok(None));
    }

    /// When two members could be meant, say so — silently taking one is
    /// the wrong-value failure this tool exists to refuse.
    #[test]
    fn a_reference_that_could_mean_two_members_is_refused_naming_both() {
        let members = [MemberRef::sheet("a#b.xlsx", "c"), MemberRef::sheet("a", "b.xlsx#c")];
        let exists = |m: &MemberRef| members.contains(m);
        let err = MemberRef::resolve("a#b.xlsx#c", exists).expect_err("ambiguous");
        assert_eq!(err, members);
    }

    #[test]
    fn a_region_member_names_its_ordinal_after_the_sheet() {
        assert_eq!(MemberRef::region("report.csv", None, 2).name(), "report.csv#2");
        assert_eq!(MemberRef::region("book.xlsx", Some("Q1".into()), 2).name(), "book.xlsx#Q1#2");
    }

    /// The rightmost segment is a region only when it is a positive integer
    /// AND such a member exists; a sheet literally called `2` beside two
    /// regions is an ambiguity, named as the sheet case already is.
    #[test]
    fn a_typed_region_reference_resolves_against_what_exists() {
        let members = [
            MemberRef::region("report.csv", None, 2),
            MemberRef::region("book.xlsx", Some("Q1".into()), 3),
            MemberRef::sheet("plain.xlsx", "2"),
        ];
        let exists = |m: &MemberRef| members.contains(m);
        assert_eq!(MemberRef::resolve("report.csv#2", exists), Ok(Some(members[0].clone())));
        assert_eq!(MemberRef::resolve("book.xlsx#Q1#3", exists), Ok(Some(members[1].clone())));
        assert_eq!(MemberRef::resolve("plain.xlsx#2", exists), Ok(Some(members[2].clone())), "a sheet called 2");
        assert_eq!(MemberRef::resolve("report.csv#0", exists), Ok(None), "regions count from 1");
        assert_eq!(MemberRef::resolve("report.csv#9", exists), Ok(None));

        let both = [MemberRef::sheet("x.xlsx", "2"), MemberRef::region("x.xlsx", None, 2)];
        let exists = |m: &MemberRef| both.contains(m);
        assert_eq!(MemberRef::resolve("x.xlsx#2", exists).expect_err("two readings"), both.to_vec());
    }
}
