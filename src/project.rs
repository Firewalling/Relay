use crate::{cli::ProjectScope, git};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectContext {
    pub invocation_directory: PathBuf,
    pub relay_project_root: PathBuf,
    pub git_repository_root: Option<PathBuf>,
    pub scope: ProjectScope,
}

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("repository scope requires a Git worktree")]
    RepositoryScopeOutsideGit,
    #[error("could not inspect Relay project path {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Git(#[from] git::GitError),
}

pub fn discover(
    invocation_directory: &Path,
    scope: ProjectScope,
) -> Result<ProjectContext, ProjectError> {
    let invocation_directory = invocation_directory.to_owned();
    let git_repository_root = git::repository_root(&invocation_directory)?;
    let relay_project_root = match scope {
        ProjectScope::Auto => {
            discover_upward(&invocation_directory, git_repository_root.as_deref())?
                .unwrap_or_else(|| invocation_directory.clone())
        }
        ProjectScope::Current => invocation_directory.clone(),
        ProjectScope::Repository => git_repository_root
            .clone()
            .ok_or(ProjectError::RepositoryScopeOutsideGit)?,
    };

    Ok(ProjectContext {
        invocation_directory,
        relay_project_root,
        git_repository_root,
        scope,
    })
}

fn discover_upward(start: &Path, git_root: Option<&Path>) -> Result<Option<PathBuf>, ProjectError> {
    let mut current = Some(start);
    while let Some(directory) = current {
        for marker in [".relay", "relay-workflows"] {
            let marker_path = directory.join(marker);
            match fs::symlink_metadata(&marker_path) {
                Ok(_) => return Ok(Some(directory.to_owned())),
                Err(source) if source.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(ProjectError::Inspect {
                        path: marker_path,
                        source,
                    });
                }
            }
        }

        if git_root.is_some_and(|root| directory == root) {
            break;
        }
        current = directory.parent();
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(directory: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(directory)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn auto_selects_nearest_relay_ancestor() {
        let repository = tempfile::tempdir().unwrap();
        git(repository.path(), &["init", "--quiet"]);
        fs::create_dir(repository.path().join(".relay")).unwrap();
        let nested_project = repository.path().join("nested");
        let invocation = nested_project.join("work/deep");
        fs::create_dir_all(&invocation).unwrap();
        fs::create_dir(nested_project.join(".relay")).unwrap();

        let context = discover(&invocation, ProjectScope::Auto).unwrap();

        assert_eq!(context.invocation_directory, invocation);
        assert_eq!(context.relay_project_root, nested_project);
        assert_eq!(
            context.git_repository_root.as_deref(),
            Some(repository.path())
        );
    }

    #[test]
    fn auto_does_not_cross_git_worktree_root() {
        let outer = tempfile::tempdir().unwrap();
        fs::create_dir(outer.path().join(".relay")).unwrap();
        let repository = outer.path().join("repository");
        let invocation = repository.join("nested");
        fs::create_dir_all(&invocation).unwrap();
        git(&repository, &["init", "--quiet"]);

        let context = discover(&invocation, ProjectScope::Auto).unwrap();

        assert_eq!(context.relay_project_root, invocation);
    }

    #[test]
    fn explicit_scopes_select_current_or_repository() {
        let repository = tempfile::tempdir().unwrap();
        git(repository.path(), &["init", "--quiet"]);
        let invocation = repository.path().join("nested");
        fs::create_dir(&invocation).unwrap();

        let current = discover(&invocation, ProjectScope::Current).unwrap();
        let repository_scope = discover(&invocation, ProjectScope::Repository).unwrap();

        assert_eq!(current.relay_project_root, invocation);
        assert_eq!(repository_scope.relay_project_root, repository.path());
    }

    #[test]
    fn repository_scope_fails_outside_git() {
        let directory = tempfile::tempdir().unwrap();

        let error = discover(directory.path(), ProjectScope::Repository).unwrap_err();

        assert!(matches!(error, ProjectError::RepositoryScopeOutsideGit));
    }
    #[test]
    fn auto_selects_workflow_project_ancestor() {
        let repository = tempfile::tempdir().unwrap();
        git(repository.path(), &["init", "--quiet"]);
        let project = repository.path().join("project");
        let invocation = project.join("nested");
        fs::create_dir_all(&invocation).unwrap();
        fs::create_dir(project.join("relay-workflows")).unwrap();

        let context = discover(&invocation, ProjectScope::Auto).unwrap();

        assert_eq!(context.relay_project_root, project);
    }
}
