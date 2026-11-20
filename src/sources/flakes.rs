//! NixOS flakes search via search.nixos.org.

use std::collections::BTreeSet;
use std::future::Future;

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::App;
use crate::text::{commas, ellipsize, err, str_of, strip};

use super::base::{EsError, es_count, es_response};

#[derive(Default)]
struct Flake {
    name: String,
    description: String,
    owner: String,
    repo: String,
    url: String,
    packages: BTreeSet<String>,
}

fn flake_query(query: &str) -> Value {
    if strip(query).is_empty() || query == "*" {
        return json!({"match_all": {}});
    }
    let wildcard = format!("*{query}*");
    json!({
        "bool": {
            "should": [
                {"match": {"flake_name": {"query": query, "boost": 3}}},
                {"match": {"flake_description": {"query": query, "boost": 2}}},
                {"match": {"package_pname": {"query": query, "boost": 1.5}}},
                {"match": {"package_description": query}},
                {"wildcard": {"flake_name": {"value": wildcard, "boost": 2.5}}},
                {"wildcard": {"package_pname": {"value": wildcard, "boost": 1}}},
                {"prefix": {"flake_name": {"value": query, "boost": 2}}}
            ],
            "minimum_should_match": 1
        }
    })
}

/// `first` unless empty, then `second`.
fn or<'a>(first: &'a str, second: &'a str) -> &'a str {
    if first.is_empty() { second } else { first }
}

fn group_hits(hits: &[Value]) -> IndexMap<String, Flake> {
    let mut flakes = IndexMap::<String, Flake>::new();
    for hit in hits {
        let src = hit.get("_source").unwrap_or(&Value::Null);
        let flake_name = strip(str_of(src, "flake_name"));
        let package_pname = str_of(src, "package_pname");
        if flake_name.is_empty() && package_pname.is_empty() {
            continue;
        }
        let description = or(str_of(src, "flake_description"), str_of(src, "package_description")).to_owned();
        let resolved = src.get("flake_resolved").filter(|resolved| resolved.is_object()).unwrap_or(&Value::Null);
        let owner = str_of(resolved, "owner");
        let repo = str_of(resolved, "repo");
        let url = str_of(resolved, "url");

        let (key, flake) = if !owner.is_empty() || !repo.is_empty() || !url.is_empty() {
            let (key, name) = if !owner.is_empty() && !repo.is_empty() {
                (format!("{owner}/{repo}"), or(flake_name, or(repo, package_pname)).to_owned())
            } else if !url.is_empty() {
                let tail = url.trim_end_matches('/').rsplit('/').next().unwrap_or("").replace(".git", "");
                (url.to_owned(), or(flake_name, or(&tail, package_pname)).to_owned())
            } else {
                let key = or(flake_name, package_pname).to_owned();
                (key.clone(), key)
            };
            let flake = Flake {
                name,
                description,
                owner: owner.into(),
                repo: repo.into(),
                url: url.into(),
                ..Flake::default()
            };
            (key, flake)
        } else if !flake_name.is_empty() {
            (flake_name.to_owned(), Flake { name: flake_name.to_owned(), description, ..Flake::default() })
        } else {
            continue;
        };

        let entry = flakes.entry(key).or_insert(flake);
        let attr_name = str_of(src, "package_attr_name");
        if !attr_name.is_empty() {
            entry.packages.insert(attr_name.to_owned());
        }
    }
    flakes
}

async fn run_search(app: &App, index: &str, query: &str, limit: i64) -> Result<String, EsError> {
    let search = json!({"bool": {"filter": [{"term": {"type": "package"}}], "must": [flake_query(query)]}});
    let body = json!({"query": search, "size": limit * 5, "track_total_hits": true});
    let data = es_response(app, index, "_search", &body).await?;
    let hits = data.pointer("/hits/hits").and_then(Value::as_array).ok_or_else(|| EsError::invalid("hits.hits"))?;
    let flakes = group_hits(hits);
    if flakes.is_empty() {
        return Ok(format!("No flakes found matching '{query}'"));
    }

    let total = data.pointer("/hits/total/value").and_then(Value::as_i64).unwrap_or(hits.len() as i64);
    let shown = flakes.len().min(limit as usize);
    let noun = if shown == 1 { "flake" } else { "flakes" };
    let mut results = vec![if total > hits.len() as i64 || shown < flakes.len() {
        format!("Showing {shown} {noun} for '{query}' ({} matching package records):\n", commas(total))
    } else {
        format!("Found {shown} {noun} matching '{query}':\n")
    }];
    for flake in flakes.values().take(shown) {
        results.push(format!("* {}", flake.name));
        if !flake.owner.is_empty() && !flake.repo.is_empty() {
            results.push(format!("  Repository: {}/{}", flake.owner, flake.repo));
        } else if !flake.url.is_empty() {
            results.push(format!("  URL: {}", flake.url));
        }
        if !flake.description.is_empty() {
            results.push(format!("  {}", ellipsize(&flake.description, 200)));
        }
        if !flake.packages.is_empty() {
            let packages: Vec<&str> = flake.packages.iter().take(5).map(String::as_str).collect();
            let suffix = if flake.packages.len() > packages.len() { ", ..." } else { "" };
            results.push(format!("  Packages: {}{suffix}", packages.join(", ")));
        }
        results.push(String::new());
    }
    Ok(strip(&results.join("\n")).to_owned())
}

/// Search NixOS flakes by name or description.
pub async fn search_flakes(app: &App, query: &str, limit: i64) -> String {
    with_flake_index(app, |index| async move { run_search(app, &index, query, limit).await }).await
}

/// Get flake ecosystem statistics.
pub async fn stats_flakes(app: &App) -> String {
    with_flake_index(app, |index| async move {
        let total = es_count(app, &index, "package").await?;
        Ok(format!("NixOS Flakes Statistics:\n* Available packages: {}", commas(total)))
    })
    .await
}

async fn with_flake_index<F>(app: &App, mut run: impl FnMut(String) -> F) -> String
where
    F: Future<Output = Result<String, EsError>> + Send,
{
    let flake_index = app.caches.channels.flake_index(&app.http).await;
    let result = run(flake_index.clone()).await;
    if result.as_ref().is_err_and(EsError::is_not_found) {
        app.caches.channels.refresh_after_404(&flake_index).await;
        let updated = app.caches.channels.flake_index(&app.http).await;
        return run(updated).await.unwrap_or_else(err);
    }
    result.unwrap_or_else(err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_group_by_repository() {
        let hits = vec![
            json!({"_source": {"flake_name": "a", "package_attr_name": "x",
                "flake_resolved": {"owner": "o", "repo": "r"}}}),
            json!({"_source": {"flake_name": "a", "package_attr_name": "y",
                "flake_resolved": {"owner": "o", "repo": "r"}}}),
            json!({"_source": {"package_pname": "p", "flake_resolved": {"url": "https://g.io/me/tool.git"}}}),
            json!({"_source": {}}),
        ];
        let flakes = group_hits(&hits);
        assert_eq!(flakes.len(), 2);
        assert_eq!(flakes["o/r"].packages.len(), 2);
        assert_eq!(flakes["https://g.io/me/tool.git"].name, "tool");
    }
}
