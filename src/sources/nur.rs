//! Community packages from the revisioned NUR search catalogue.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde::Deserialize;
use tokio::sync::OnceCell;

use crate::App;
use crate::http::Http;
use crate::text::{ellipsize, error, score_option_match, str_of};

const INDEX_REF: &str = "https://api.github.com/repos/nix-community/nur-search/git/ref/heads/main";
const REPOSITORIES: &str = "https://raw.githubusercontent.com/nix-community/NUR/master/repos.json";

#[derive(Deserialize)]
struct Package {
    #[serde(rename = "_repo")]
    repository: String,
    version: String,
    meta: Metadata,
}

#[derive(Deserialize)]
struct Metadata {
    description: Option<String>,
    position: Option<String>,
}

struct Catalogue {
    revision: String,
    packages: BTreeMap<String, Package>,
}

#[derive(Deserialize)]
struct Repositories {
    repos: BTreeMap<String, Repository>,
}

#[derive(Deserialize)]
struct Repository {
    url: String,
}

#[derive(Default)]
pub(crate) struct NurCache {
    catalogue: OnceCell<Catalogue>,
    repositories: OnceCell<Repositories>,
}

async fn load(http: &Http) -> Result<Catalogue, String> {
    let request = http.get(INDEX_REF);
    let head = http.send(request, 10).await.and_then(|r| r.ok()?.json()).map_err(|e| e.to_string())?;
    let revision = str_of(&head["object"], "sha").to_owned();
    if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("NUR search index returned an invalid revision".into());
    }
    let url = format!("https://raw.githubusercontent.com/nix-community/nur-search/{revision}/data/packages.json");
    let body = http.send(http.get(&url), 60).await.and_then(|r| r.ok()).map_err(|e| e.to_string())?.body;
    let packages: BTreeMap<String, Package> = tokio::task::spawn_blocking(move || serde_json::from_slice(&body))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Invalid NUR catalogue: {e}"))?;
    if packages.is_empty() {
        return Err("NUR package catalogue is empty".into());
    }
    Ok(Catalogue { revision, packages })
}

async fn catalogue(app: &App) -> Result<&Catalogue, String> {
    app.caches.nur.catalogue.get_or_try_init(|| load(&app.http)).await
}

pub async fn search(app: &App, query: &str, limit: usize) -> String {
    let catalogue = match catalogue(app).await {
        Ok(catalogue) => catalogue,
        Err(e) => return error(e, "API_ERROR"),
    };
    let mut matches: Vec<_> = catalogue
        .packages
        .iter()
        .filter_map(|(attr, package)| {
            let description = package.meta.description.as_deref().unwrap_or_default();
            let score = score_option_match(attr, description, query);
            (score > 0).then_some((Reverse(score), attr))
        })
        .collect();
    matches.sort_unstable();
    let mut lines = vec![
        format!("NUR community packages matching '{query}' ({} matches)", matches.len()),
        format!("Index revision: {}", catalogue.revision),
    ];
    for (_, attr) in matches.into_iter().take(limit) {
        let p = &catalogue.packages[attr];
        lines.push(format!("* {attr} ({})", p.version));
        if let Some(desc) = &p.meta.description {
            lines.push(format!("  {}", ellipsize(desc, 200)));
        }
    }
    lines.join("\n")
}

pub async fn info(app: &App, attr: &str) -> String {
    let catalogue = match catalogue(app).await {
        Ok(catalogue) => catalogue,
        Err(e) => return error(e, "API_ERROR"),
    };
    let Some(package) = catalogue.packages.get(attr) else {
        return error(
            format!("NUR package '{attr}' not found. Use the full nur.repos.<repo>.<attr> path."),
            "NOT_FOUND",
        );
    };
    let mut lines = vec![
        format!("NUR community package: {attr}"),
        format!("Version: {}", package.version),
        format!("NUR repository: {}", package.repository),
        format!("Index revision: {}", catalogue.revision),
    ];
    let registry = app
        .caches
        .nur
        .repositories
        .get_or_try_init(|| async {
            let response = app.http.send(app.http.get(REPOSITORIES), 15).await?.ok()?;
            serde_json::from_slice::<Repositories>(&response.body)
                .map_err(|e| crate::http::FetchError::Parse(e.to_string()))
        })
        .await;
    if let Some(repo) = registry.ok().and_then(|r| r.repos.get(&package.repository)) {
        lines.push(format!("Repository URL: {}", repo.url));
    }
    if let Some(position) = &package.meta.position {
        lines.push(format!("Definition: {position}"));
    }
    if let Some(desc) = &package.meta.description {
        lines.push(format!("Description: {desc}"));
    }
    lines.join("\n")
}
