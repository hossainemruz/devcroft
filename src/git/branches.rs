//! Bounded branch discovery; full refs are retained for unambiguous operations.
use super::repository::{LoadError, run_git_args, stderr_message};
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Branch {
    pub reference: String,
    pub name: String,
    pub remote: bool,
    pub current: bool,
    pub oid: String,
    pub subject: String,
    pub upstream: String,
    pub tracking: String,
    pub push_remote: String,
    pub push_ref: String,
}

pub(super) fn load_branches(root: &Path) -> Result<Vec<Branch>, LoadError> {
    let output = run_git_args(root, ["for-each-ref", "--sort=refname", "--format=%(refname)%00%(HEAD)%00%(objectname)%00%(subject)%00%(upstream:short)%00%(upstream:track)%00%(symref)%00%(upstream:remotename)%00%(upstream:remoteref)", "refs/heads/", "refs/remotes/"].into_iter().map(Into::into).collect(), None, Duration::from_secs(30), 8 * 1024 * 1024, 2048)?;
    if !output.status.success() {
        return Err(LoadError::Failed(stderr_message(&output)));
    }
    if output.stdout_truncated {
        return Err(LoadError::Failed(
            "Branch list is too large to display safely".into(),
        ));
    }
    parse_branches(&output.stdout)
}

fn parse_branches(bytes: &[u8]) -> Result<Vec<Branch>, LoadError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| LoadError::Failed("Branch names are not valid UTF-8".into()))?;
    let mut branches = Vec::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split('\0').collect();
        if fields.len() != 9 {
            return Err(LoadError::Failed("Could not parse Git branches".into()));
        }
        if !fields[6].is_empty() {
            continue;
        }
        let (name, remote) = if let Some(name) = fields[0].strip_prefix("refs/heads/") {
            (name, false)
        } else if let Some(name) = fields[0].strip_prefix("refs/remotes/") {
            (name, true)
        } else {
            continue;
        };
        branches.push(Branch {
            reference: fields[0].into(),
            name: name.into(),
            remote,
            current: fields[1] == "*",
            oid: fields[2].into(),
            subject: fields[3].into(),
            upstream: fields[4].into(),
            tracking: fields[5].into(),
            push_remote: fields[7].into(),
            push_ref: fields[8].into(),
        });
    }
    Ok(branches)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_full_refs_and_skips_remote_head_aliases() {
        let branches = parse_branches(b"refs/heads/topic\0*\0abc\0subject\0origin/topic\0[ahead 2, behind 1]\0\0origin\0refs/heads/topic\nrefs/remotes/origin/HEAD\0 \0abc\0subject\0\0\0refs/remotes/origin/main\0\0\n").unwrap();
        assert_eq!(branches.len(), 1);
        assert!(branches[0].current);
        assert_eq!(branches[0].push_ref, "refs/heads/topic");
        assert_eq!(branches[0].tracking, "[ahead 2, behind 1]");
    }
}
