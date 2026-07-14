//! diff a branch/worktree revision against the repo's base branch

use super::commit_files::OldNew;
use super::{repo, CommitId, RepoPath};
use crate::error::{Error, Result};
use scopetime::scope_time;

/// The merge-base (most-recent common ancestor) of two commits.
pub fn merge_base(
	repo_path: &RepoPath,
	one: CommitId,
	two: CommitId,
) -> Result<CommitId> {
	scope_time!("merge_base");
	let repo = repo(repo_path)?;
	let oid = repo.merge_base(one.get_oid(), two.get_oid())?;
	Ok(CommitId::new(oid))
}

/// base-branch revisions tried, in priority order.
const BASE_BRANCH_CANDIDATES: &[&str] = &[
	"main",
	"master",
	"origin/main",
	"origin/master",
	"origin/HEAD",
];

/// Resolves the repository's base branch to a commit, trying `main`,
/// `master`, and their `origin/` counterparts in order.
pub fn resolve_base_branch(repo_path: &RepoPath) -> Result<CommitId> {
	scope_time!("resolve_base_branch");
	for candidate in BASE_BRANCH_CANDIDATES {
		if let Ok(id) = CommitId::from_revision(repo_path, candidate)
		{
			return Ok(id);
		}
	}
	Err(Error::Generic(
		"no base branch (main/master) found".to_string(),
	))
}

/// Commit range for reviewing `revision` (a branch name or "HEAD")
/// against `base`: `old` = merge-base(base, tip of `revision`),
/// `new` = tip of `revision`.
fn diff_range(
	repo_path: &RepoPath,
	revision: &str,
	base: CommitId,
) -> Result<OldNew<CommitId>> {
	let new = CommitId::from_revision(repo_path, revision)?;
	let old = merge_base(repo_path, base, new)?;
	Ok(OldNew { old, new })
}

/// Commit range for reviewing `revision` against the auto-resolved
/// base branch (main/master).
///
/// `old` = merge-base(base, tip), `new` = tip of `revision`. Feed to
/// the compare-commits view.
pub fn diff_range_vs_base(
	repo_path: &RepoPath,
	revision: &str,
) -> Result<OldNew<CommitId>> {
	scope_time!("diff_range_vs_base");
	let base = resolve_base_branch(repo_path)?;
	diff_range(repo_path, revision, base)
}

/// Commit range for reviewing `revision` against an explicit
/// `base_revision` (a branch name, tag, or revspec).
///
/// `old` = merge-base(base, tip), `new` = tip of `revision`. Feed to
/// the compare-commits view.
pub fn diff_range_vs_ref(
	repo_path: &RepoPath,
	revision: &str,
	base_revision: &str,
) -> Result<OldNew<CommitId>> {
	scope_time!("diff_range_vs_ref");
	let base = CommitId::from_revision(repo_path, base_revision)?;
	diff_range(repo_path, revision, base)
}

#[cfg(test)]
mod tests {
	use super::{
		diff_range_vs_base, diff_range_vs_ref, merge_base,
		resolve_base_branch,
	};
	use crate::sync::{
		branch::{checkout_branch, create_branch},
		commit_files::get_commit_files,
		tests::{repo_init, write_commit_file},
		RepoPath,
	};
	use pretty_assertions::assert_eq;

	#[test]
	fn test_merge_base_linear_history() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath = root.to_str().unwrap().into();

		let commit_a =
			write_commit_file(&repo, "a.txt", "a", "commit a");
		let commit_b =
			write_commit_file(&repo, "b.txt", "b", "commit b");

		let base =
			merge_base(&repo_path, commit_a, commit_b).unwrap();

		assert_eq!(base, commit_a);
	}

	#[test]
	fn test_resolve_base_branch_finds_master() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath = root.to_str().unwrap().into();

		assert!(resolve_base_branch(&repo_path).is_ok());
	}

	#[test]
	fn test_diff_range_vs_base() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath = root.to_str().unwrap().into();

		let base_commit = write_commit_file(
			&repo,
			"base.txt",
			"base",
			"base commit",
		);

		create_branch(&repo_path, "feature").unwrap();
		checkout_branch(&repo_path, "feature").unwrap();

		let feature_commit = write_commit_file(
			&repo,
			"feature.txt",
			"feature",
			"feature commit",
		);

		let range =
			diff_range_vs_base(&repo_path, "feature").unwrap();

		assert_eq!(range.old, base_commit);
		assert_eq!(range.new, feature_commit);

		let files =
			get_commit_files(&repo_path, range.new, Some(range.old))
				.unwrap();

		assert_eq!(files.len(), 1);
		assert_eq!(files[0].path, "feature.txt");
	}

	#[test]
	fn test_diff_range_vs_ref_explicit_base() {
		let (_td, repo) = repo_init().unwrap();
		let root = repo.path().parent().unwrap();
		let repo_path: RepoPath = root.to_str().unwrap().into();

		let base_commit = write_commit_file(
			&repo,
			"base.txt",
			"base",
			"base commit",
		);

		create_branch(&repo_path, "feature").unwrap();
		checkout_branch(&repo_path, "feature").unwrap();

		let feature_commit = write_commit_file(
			&repo,
			"feature.txt",
			"feature",
			"feature commit",
		);

		// HEAD (feature) diffed against an explicit base branch
		let range =
			diff_range_vs_ref(&repo_path, "HEAD", "master").unwrap();

		assert_eq!(range.old, base_commit);
		assert_eq!(range.new, feature_commit);
	}
}
