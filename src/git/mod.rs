use std::{num::NonZeroU32, path::Path, sync::atomic::AtomicBool};

use anyhow::{Context, Result};

/// Clone the remote's default branch and check it out with one commit of history.
pub(crate) fn shallow_clone(url: &str, destination: &Path) -> Result<()> {
    let interrupt = AtomicBool::new(false);
    // Daemon clones must not launch configured credential helpers or prompt on stdin.
    // A token is injected by the service manager as an environment variable instead.
    let github_token = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    let has_github_token = github_token.is_some();
    let mut prepare = gix::clone::PrepareFetch::new(
        url,
        destination,
        gix::create::Kind::WithWorktree,
        gix::create::Options::default(),
        gix::open::Options::default(),
    )
    .with_context(|| format!("initialize repository clone from {url}"))?
    .with_shallow(gix::remote::fetch::Shallow::DepthAtRemote(
        NonZeroU32::new(1).expect("one is non-zero"),
    ))
    .configure_connection(move |connection| {
        let token = github_token.clone();
        connection.set_credentials(move |action| match action {
            gix::credentials::helper::Action::Get(context)
                if context.url.as_ref().is_some_and(|url| {
                    is_github_https_url(&String::from_utf8_lossy(url.as_ref()))
                }) =>
            {
                let Some(token) = token.as_ref() else {
                    return Ok(None);
                };
                Ok(Some(gix::credentials::protocol::Outcome {
                    identity: gix::sec::identity::Account {
                        username: "x-access-token".to_owned(),
                        password: token.clone(),
                        oauth_refresh_token: None,
                    },
                    next: gix::credentials::helper::NextAction::from(context),
                }))
            }
            gix::credentials::helper::Action::Get(_)
            | gix::credentials::helper::Action::Store(_)
            | gix::credentials::helper::Action::Erase(_) => Ok(None),
        });
        Ok(())
    });

    let (mut checkout, _) = prepare
        .fetch_then_checkout(gix::progress::Discard, &interrupt)
        .with_context(|| {
            if !has_github_token && url.starts_with("https://github.com/") {
                format!("fetch shallow repository from {url}; if it is private, set GITHUB_TOKEN (credential helpers and interactive prompts are disabled)")
            } else {
                format!("fetch shallow repository from {url}")
            }
        })?;
    checkout
        .main_worktree(gix::progress::Discard, &interrupt)
        .with_context(|| format!("check out shallow repository from {url}"))?;
    Ok(())
}

fn is_github_https_url(url: &str) -> bool {
    let Some(authority_and_path) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    authority.eq_ignore_ascii_case("github.com")
}
