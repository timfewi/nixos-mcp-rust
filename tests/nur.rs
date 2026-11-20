mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn nursery_index_is_loaded_lazily_at_the_reported_revision() {
    const REV: &str = "6eb5e7ee2a43cb0b674effbdecd1bf4421da3d68";
    let fixture = serve(|request| match request.path.as_str() {
        "/api.github.com/repos/nix-community/nur-search/git/ref/heads/main" => {
            json_reply(json!({"object":{"sha":REV}}))
        }
        "/raw.githubusercontent.com/nix-community/NUR/master/repos.json" => {
            json_reply(json!({"repos":{"example":{"url":"https://example.org/nur"}}}))
        }
        path if path == format!("/raw.githubusercontent.com/nix-community/nur-search/{REV}/data/packages.json") => {
            json_reply(json!({
                "nur.repos.example.editor":{"_repo":"example","version":"1.0","meta":{"description":"Text editor","position":"https://example.org/definition"}},
                "nur.repos.other.gui":{"_repo":"other","version":"2.0","meta":{"description":"Graphical editor"}}
            }))
        }
        _ => status(404),
    });
    let app = fixture.app();
    assert_eq!(fixture.count("github"), 0);
    let text = call(&app, json!({"action":"search","source":"nur","query":"editor","limit":1})).await;
    assert!(
        text.contains(REV) && text.contains("* nur.repos.example.editor (1.0)") && !text.contains("other.gui"),
        "{text}"
    );
    let text = call(&app, json!({"action":"info","source":"nur","query":"nur.repos.example.editor"})).await;
    assert!(
        text.contains("Repository URL: https://example.org/nur")
            && text.contains("Definition: https://example.org/definition"),
        "{text}"
    );
    assert_eq!(fixture.count("data/packages.json"), 1);
}
