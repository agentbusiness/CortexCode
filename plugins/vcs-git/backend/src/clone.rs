//! `vcs.clone`: a repository into a new folder through libgit2 (no git program needed). GitHub
//! repositories use the connection's token when there is one, so private ones clone too; the token
//! is handed to libgit2 only when GitHub asks for it and is never written into the clone.

use crate::github::{Github, split_full_name};
use cc_protocol::contracts::vcs::CloneResult;
use cc_protocol::{RpcError, codes};
use std::path::{Path, PathBuf};

/// The clone address, the folder name and whether it is on GitHub, from `owner/name` or an https URL.
fn source(repo: &str) -> Result<(String, String, bool), RpcError> {
    let repo = repo.trim().trim_end_matches('/');
    if repo.contains("://") || repo.starts_with("git@") {
        let url = reqwest::Url::parse(repo)
            .ok()
            .filter(|url| url.scheme() == "https")
            .ok_or_else(|| RpcError::invalid_params(format!("`{repo}` is not an https address; use owner/name or https://…")))?;
        let name = url
            .path_segments()
            .and_then(|mut segments| segments.rfind(|s| !s.is_empty()))
            .map(|last| last.trim_end_matches(".git").to_owned())
            .filter(|name| !name.is_empty() && name != "." && name != "..")
            .ok_or_else(|| RpcError::invalid_params(format!("`{repo}` names no repository")))?;
        let on_github = url.host_str().is_some_and(|host| host.eq_ignore_ascii_case("github.com"));
        return Ok((url.to_string(), name, on_github));
    }
    let (owner, name) = split_full_name(repo)?;
    let name = name.trim_end_matches(".git");
    Ok((format!("https://github.com/{owner}/{name}.git"), name.to_owned(), true))
}

fn fetch(url: &str, dest: &Path, token: Option<String>) -> Result<(), git2::Error> {
    let mut callbacks = git2::RemoteCallbacks::new();
    if let Some(token) = token {
        // libgit2 asks again after a refused credential: answer once, then fail.
        let mut offered = false;
        callbacks.credentials(move |_url, _user, _allowed| {
            if offered {
                return Err(git2::Error::from_str("GitHub did not accept the saved sign-in"));
            }
            offered = true;
            git2::Cred::userpass_plaintext("x-access-token", &token)
        });
    }
    let mut options = git2::FetchOptions::new();
    options.remote_callbacks(callbacks);
    git2::build::RepoBuilder::new().fetch_options(options).clone(url, dest).map(|_| ())
}

pub async fn clone(github: &Github, repo: &str, parent: Option<&str>) -> Result<CloneResult, RpcError> {
    let (url, name, on_github) = source(repo)?;
    let parent = match parent.map(str::trim).filter(|p| !p.is_empty()) {
        Some(parent) if Path::new(parent).is_absolute() => PathBuf::from(parent),
        Some(parent) => return Err(RpcError::invalid_params(format!("`parent` must be an absolute path, got `{parent}`"))),
        None => github
            .dev_folder()
            .ok_or_else(|| RpcError::invalid_params("choose a dev folder first: the folder new clones go into"))?,
    };
    let dest = parent.join(&name);
    let occupied = dest.exists() && (!dest.is_dir() || std::fs::read_dir(&dest).map(|mut d| d.next().is_some()).unwrap_or(true));
    if occupied {
        return Err(RpcError::invalid_params(format!("{} already exists and is not empty", dest.display())));
    }
    std::fs::create_dir_all(&parent)
        .map_err(|error| RpcError::invalid_params(format!("cannot create {}: {error}", parent.display())))?;
    let token = if on_github { github.clone_token().await } else { None };
    let signed_in = token.is_some();
    let created = !dest.exists();
    let target = dest.clone();
    let outcome = tokio::task::spawn_blocking(move || fetch(&url, &target, token))
        .await
        .map_err(|error| RpcError::internal(format!("clone task: {error}")))?;
    if let Err(error) = outcome {
        // A failed clone leaves no half-made folder behind (only one this call made).
        if created {
            let _ = std::fs::remove_dir_all(&dest);
        }
        let message = if error.code() == git2::ErrorCode::Auth || error.class() == git2::ErrorClass::Http {
            if signed_in {
                format!("{repo} does not exist, or this GitHub account cannot read it")
            } else if on_github {
                format!("{repo} is private or does not exist; connect GitHub in Settings > GitHub to clone private repositories")
            } else {
                format!("{repo} needs a sign-in, or does not exist")
            }
        } else if error.class() == git2::ErrorClass::Net {
            return Err(RpcError::new(codes::UNAVAILABLE, format!("could not reach the server for {repo}: {}", error.message()))
                .retryable());
        } else {
            format!("could not clone {repo}: {}", error.message())
        };
        return Err(RpcError::invalid_params(message));
    }
    let path = std::fs::canonicalize(&dest).unwrap_or(dest);
    Ok(CloneResult { path: path.to_string_lossy().into_owned() })
}
