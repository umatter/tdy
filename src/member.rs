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
}

impl MemberRef {
    pub fn file(path: impl Into<String>) -> MemberRef {
        MemberRef { path: path.into(), sheet: None }
    }

    pub fn sheet(path: impl Into<String>, sheet: impl Into<String>) -> MemberRef {
        MemberRef { path: path.into(), sheet: Some(sheet.into()) }
    }

    /// The form a person reads and types: `path`, or `path#sheet`.
    pub fn name(&self) -> String {
        match &self.sheet {
            Some(s) => format!("{}#{s}", self.path),
            None => self.path.clone(),
        }
    }

    /// Resolve a typed reference against the members that exist. Every
    /// split at a `#` is a candidate — the whole text as a plain member,
    /// then each `path#sheet` split from the right — and the first that
    /// `exists` is the answer; `Ok(None)` names no member, and `Err` carries
    /// every member the text could mean when there is more than one.
    pub fn resolve(text: &str, exists: impl Fn(&MemberRef) -> bool) -> Result<Option<MemberRef>, Vec<MemberRef>> {
        let mut found: Vec<MemberRef> = Vec::new();
        let plain = MemberRef::file(text);
        if exists(&plain) {
            found.push(plain);
        }
        for (i, _) in text.rmatch_indices('#') {
            let candidate = MemberRef::sheet(&text[..i], &text[i + 1..]);
            if exists(&candidate) {
                found.push(candidate);
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
}
