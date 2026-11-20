//! Channel discovery, resolution and failure handling.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use mcp_nixos::config::FLAKE_INDEX;
use serde_json::json;

fn aliases(names: &[&str]) -> Reply {
    json_reply(names.iter().map(|name| json!({"alias": name})).collect())
}

#[tokio::test]
async fn resolves_highest_generation_and_newest_release() {
    let fixture = serve(|request| search_backend(request).unwrap_or_else(|| status(404)));
    let app = fixture.app();
    let channels = app.caches.channels.resolved(&app.http).await;
    assert_eq!(channels["unstable"], "latest-50-nixos-unstable");
    assert_eq!(channels["stable"], "latest-50-nixos-26.05");
    assert_eq!(channels["beta"], "latest-50-nixos-26.05");
    assert_eq!(channels["25.11"], "latest-48-nixos-25.11");
    assert_eq!(channels.keys().next().map(String::as_str), Some("unstable"));
}

#[tokio::test]
async fn resolution_is_memoized() {
    let fixture = serve(|request| search_backend(request).unwrap_or_else(|| status(404)));
    let app = fixture.app();
    app.caches.channels.resolved(&app.http).await;
    app.caches.channels.resolved(&app.http).await;
    assert_eq!(fixture.count("_cat/aliases"), 1);
}

#[tokio::test]
async fn confirmed_empty_alias_is_never_resolved() {
    let fixture = serve(|request| {
        if request.path.contains("_cat/aliases") {
            aliases(&["latest-50-nixos-unstable", "latest-51-nixos-unstable"])
        } else if request.path.contains("latest-51") {
            json_reply(json!({"count": 0}))
        } else {
            json_reply(json!({"count": 5}))
        }
    });
    let app = fixture.app();
    assert_eq!(app.caches.channels.resolved(&app.http).await["unstable"], "latest-50-nixos-unstable");
}

#[tokio::test]
async fn failed_count_probes_keep_channels_resolvable_but_uncached() {
    let fixture = serve(|request| {
        if request.path.contains("_cat/aliases") {
            aliases(&["latest-50-nixos-unstable", "latest-50-nixos-26.05"])
        } else {
            status(503)
        }
    });
    let app = fixture.app();
    let first = app.caches.channels.resolved(&app.http).await;
    assert_eq!(first["unstable"], "latest-50-nixos-unstable");
    assert!(first.contains_key("stable"));
    app.caches.channels.resolved(&app.http).await;
    assert!(fixture.count("_count") >= 4, "a partial listing must be re-probed");
    assert_eq!(fixture.count("_cat/aliases"), 1, "alias names are cached independently of counts");
}

#[tokio::test]
async fn failed_discovery_uses_fallback_without_memoizing_and_backs_off() {
    let healthy = Arc::new(AtomicBool::new(false));
    let flag = healthy.clone();
    let fixture = serve(move |request| {
        if flag.load(Ordering::SeqCst) { search_backend(request).unwrap_or_else(|| status(404)) } else { status(500) }
    });
    let app = fixture.app();
    let channels = &app.caches.channels;
    let fallback = channels.resolved(&app.http).await;
    assert_eq!(fallback["unstable"], "latest-50-nixos-unstable");
    assert!(channels.using_fallback().await);
    channels.resolved(&app.http).await;
    assert_eq!(fixture.count("_cat/aliases"), 1, "cooldown must stop a retry storm");

    healthy.store(true, Ordering::SeqCst);
    let flake_index = channels.flake_index(&app.http).await;
    assert_eq!(flake_index, FLAKE_INDEX, "the cooldown also covers the flake alias");
}

#[tokio::test]
async fn flake_index_comes_from_the_alias_probe() {
    let fixture = serve(|request| search_backend(request).unwrap_or_else(|| status(404)));
    let app = fixture.app();
    assert_eq!(app.caches.channels.flake_index(&app.http).await, "latest-51-group-manual");
}

#[tokio::test]
async fn channel_listing_labels_indexed_commits_and_branch_heads() {
    let fixture = serve(move |request| {
        if request.path.contains("_cat/aliases") {
            return json_reply(json!([
                {"alias": "latest-50-nixos-unstable", "index": format!("nixos-50-unstable-{}", "a".repeat(40))},
                {"alias": "latest-50-nixos-26.05", "index": "nixos-50-26.05-latest"},
                {"alias": "latest-48-nixos-25.11", "index": format!("nixos-48-25.11-{}", "b".repeat(40))},
            ]));
        }
        if request.path.contains("api.github.com") {
            return json_reply(json!({"sha": "c".repeat(40)}));
        }
        search_backend(request).unwrap_or_else(|| status(404))
    });
    let app = fixture.app();
    let text = call(&app, json!({"action": "channels"})).await;
    assert!(text.starts_with("NixOS Channels:"), "{text}");
    assert!(text.contains("* stable (current: 26.05) -> latest-50-nixos-26.05"), "{text}");
    assert!(text.contains("Status: Available (1,234 documents)"), "{text}");
    assert!(text.contains("Branch: nixos-unstable"), "{text}");
    assert!(text.contains(&format!("Revision (indexed): {}", "a".repeat(40))));
    assert!(text.contains(&format!("Revision (indexed): {}", "b".repeat(40))));
    assert!(text.contains(&format!("Branch HEAD: {} (upstream; may be ahead of indexed data)", "c".repeat(40))));

    call(&app, json!({"action": "channels"})).await;
    assert_eq!(fixture.count("commits/nixos-unstable"), 0, "the alias target contains the indexed revision");
    assert_eq!(fixture.count("commits/nixos-26.05"), 1, "the TTL cache must answer repeat listings");
}

#[tokio::test]
async fn retired_nixos_alias_is_rediscovered_and_retried() {
    for args in [
        json!({"action": "search", "query": "hello"}),
        json!({"action": "info", "query": "hello"}),
        json!({"action": "info", "query": "services.hello.enable", "type": "option"}),
        json!({"action": "stats"}),
    ] {
        let retired = Arc::new(AtomicBool::new(false));
        let flag = retired.clone();
        let fixture = serve(move |request| {
            if request.path.contains("_cat/aliases") {
                return aliases(&[if flag.load(Ordering::SeqCst) {
                    "latest-51-nixos-unstable"
                } else {
                    "latest-50-nixos-unstable"
                }]);
            }
            if request.path.contains("latest-50") && flag.load(Ordering::SeqCst) {
                return status(404);
            }
            if request.path.ends_with("/_count") {
                return json_reply(json!({"count": 5}));
            }
            es_hits(vec![json!({
                "package_pname": "hello",
                "package_pversion": "2.0",
                "option_name": "services.hello.enable",
            })])
        });
        let app = fixture.app();
        let expected = call(&app, args.clone()).await;
        assert!(!expected.starts_with("Error"), "{expected}");
        retired.store(true, Ordering::SeqCst);
        assert_eq!(call(&app, args).await, expected);
        assert_eq!(fixture.count("_cat/aliases"), 2);
    }
}

#[tokio::test]
async fn retired_flake_alias_is_rediscovered_and_retried() {
    let retired = Arc::new(AtomicBool::new(false));
    let flag = retired.clone();
    let fixture = serve(move |request| {
        if request.path.contains("_cat/aliases") {
            return aliases(&[if flag.load(Ordering::SeqCst) {
                "latest-52-group-manual"
            } else {
                "latest-51-group-manual"
            }]);
        }
        if request.path.contains("latest-51") && flag.load(Ordering::SeqCst) {
            return status(404);
        }
        json_reply(json!({"count": 42}))
    });
    let app = fixture.app();
    let args = json!({"action": "stats", "source": "flakes"});
    let expected = "NixOS Flakes Statistics:\n* Available packages: 42";
    assert_eq!(call(&app, args.clone()).await, expected);
    retired.store(true, Ordering::SeqCst);
    assert_eq!(call(&app, args).await, expected);
    assert_eq!(fixture.count("_cat/aliases"), 2);
}

#[tokio::test]
async fn invalid_channels_get_suggestions() {
    let fixture = serve(|request| search_backend(request).unwrap_or_else(|| status(404)));
    let app = fixture.app();
    let text = call(&app, json!({"action": "search", "query": "x", "channel": "unstabl"})).await;
    assert_eq!(text, "Error (ERROR): Invalid channel 'unstabl'. Available channels: unstable");
    let text = call(&app, json!({"action": "stats", "channel": "nope"})).await;
    assert_eq!(text, "Error (ERROR): Invalid channel 'nope'. Available channels: unstable, stable, beta, 26.05, 25.11");
}
