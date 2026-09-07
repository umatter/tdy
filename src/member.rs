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
    /// `exists` is the answer. `None` names no member.
    pub fn resolve(text: &str, exists: impl Fn(&MemberRef) -> bool) -> Option<MemberRef> {
        let plain = MemberRef::file(text);
        if exists(&plain) {
            return Some(plain);
        }
        for (i, _) in text.rmatch_indices('#') {
            let candidate = MemberRef::sheet(&text[..i], &text[i + 1..]);
            if exists(&candidate) {
                return Some(candidate);
            }
        }
        None
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
        let members = vec![
            MemberRef::sheet("2025#final.xlsx", "Q1"),
            MemberRef::sheet("2025.xlsx", "Q#2"),
            MemberRef::file("plain#name.csv"),
        ];
        let exists = |m: &MemberRef| members.contains(m);
        assert_eq!(MemberRef::resolve("2025#final.xlsx#Q1", exists), Some(members[0].clone()));
        assert_eq!(MemberRef::resolve("2025.xlsx#Q#2", exists), Some(members[1].clone()));
        assert_eq!(MemberRef::resolve("plain#name.csv", exists), Some(members[2].clone()));
        assert_eq!(MemberRef::resolve("2025.xlsx#Q3", exists), None);
        assert_eq!(MemberRef::resolve("nothing.csv", exists), None);
    }
}
