mod common;

use common::{call, ok, serve, status};
use serde_json::json;

const HOST_PATH: &str = "/microvm-nix.github.io/microvm.nix/host-options.html";
const GUEST_PATH: &str = "/microvm-nix.github.io/microvm.nix/microvm-options.html";
const FLAKE_PARTS_PATH: &str = "/flake.parts/options/flake-parts.html";

const HOST_OPTIONS: &str = r##"<main>
<h2 id="microvmvms"><a class="header" href="#microvmvms">microvm.vms</a></h2>
<p>Declare virtual machines on this host.</p>
<p><em>Type:</em>
attribute set of (submodule)</p>
<p><em>Default:</em></p>
<pre><code class="language-nix">{ }
</code></pre>
<p><em>Declared by:</em></p>
<ul><li><a href="https://github.com/microvm-nix/microvm.nix/tree/main/nixos-modules/host/options.nix">microvm.nix/nixos-modules/host/options.nix</a></li></ul>
<h2 id="microvmvmsnameautostart"><a class="header" href="#microvmvmsnameautostart">microvm.vms.&lt;name&gt;.autostart</a></h2>
<p>Start the guest when the host boots.</p>
<p><em>Type:</em> boolean</p>
<p><em>Default:</em></p>
<pre><code class="language-nix">true
</code></pre>
</main>"##;

const GUEST_OPTIONS: &str = r##"<main>
<h2 id="microvmhypervisor"><a class="header" href="#microvmhypervisor">microvm.hypervisor</a></h2>
<p>Select the hypervisor for this guest.</p>
<p><em>Type:</em> one of “qemu”, “firecracker”</p>
<p><em>Default:</em></p>
<pre><code class="language-nix">"qemu"
</code></pre>
<p><em>Declared by:</em></p>
<ul><li><a href="https://github.com/microvm-nix/microvm.nix/tree/main/nixos-modules/microvm/options.nix">microvm.nix/nixos-modules/microvm/options.nix</a></li></ul>
</main>"##;

const FLAKE_PARTS_OPTIONS: &str = r##"<main>
<h1 id="core-options"><a class="header" href="#core-options">Core Options</a></h1>
<h2 id="options"><a class="header" href="#options">Options</a></h2>
<h2 id="opt-perSystem.packages"><a class="header" href="#opt-perSystem.packages">perSystem.packages</a></h2>
<p>Packages that can be built by <code>nix build</code>.</p>
<p><em>Type:</em> lazy attribute set of package</p>
<p><em>Default:</em></p>
<pre><code class="language-nix">{ }
</code></pre>
<p><em>Example:</em></p>
<pre><code class="language-nix">{
  hello = pkgs.hello;
}
</code></pre>
<p><em>Declared by:</em></p>
<ul><li><a href="https://github.com/hercules-ci/flake-parts/blob/main/modules/packages.nix">flake-parts/modules/packages.nix</a></li></ul>
</main>"##;

#[tokio::test]
async fn microvm_exposes_documented_host_and_guest_options() {
    let fixture = serve(|request| match request.path.as_str() {
        HOST_PATH => ok(HOST_OPTIONS),
        GUEST_PATH => ok(GUEST_OPTIONS),
        _ => status(404),
    });
    let app = fixture.app();

    let search = call(&app, json!({"action": "search", "source": "microvm", "query": "microvm.vms", "limit": 1})).await;
    assert!(search.contains("* microvm.vms (MicroVM.nix host)"), "{search}");
    assert!(!search.contains("autostart"), "{search}");

    let guest = call(&app, json!({"action": "info", "source": "microvm", "query": "microvm.hypervisor"})).await;
    assert!(guest.contains("Module: MicroVM.nix guest"), "{guest}");
    assert!(guest.contains("Default: \"qemu\""), "{guest}");
    assert!(guest.contains("microvm-options.html#microvmhypervisor"), "{guest}");

    let host =
        call(&app, json!({"action": "info", "source": "microvm", "query": "microvm.vms.<name>.autostart"})).await;
    assert!(host.contains("Module: MicroVM.nix host"), "{host}");
    assert!(host.contains("Default: true"), "{host}");

    let browse = call(&app, json!({"action": "browse", "source": "microvm", "query": "microvm.vms."})).await;
    assert!(browse.contains("microvm.vms.<name>.autostart"), "{browse}");
    assert!(!browse.contains("microvm.hypervisor"), "{browse}");
    let stats = call(&app, json!({"action": "stats", "source": "microvm"})).await;
    assert!(stats.contains("Total options: 3"), "{stats}");
    assert_eq!(fixture.count(HOST_PATH), 1);
    assert_eq!(fixture.count(GUEST_PATH), 1);
}

#[tokio::test]
async fn flake_parts_core_exposes_option_fields_and_excludes_section_headings() {
    let fixture = serve(|request| match request.path.as_str() {
        FLAKE_PARTS_PATH => ok(FLAKE_PARTS_OPTIONS),
        _ => status(404),
    });
    let app = fixture.app();

    let info = call(&app, json!({"action": "info", "source": "flake-parts", "query": "perSystem.packages"})).await;
    assert!(info.contains("Description: Packages that can be built by nix build."), "{info}");
    assert!(info.contains("Type: lazy attribute set of package"), "{info}");
    assert!(info.contains("Default: { }"), "{info}");
    assert!(info.contains("Example: {\n  hello = pkgs.hello;\n}"), "{info}");
    assert!(
        info.contains("Declared in: https://github.com/hercules-ci/flake-parts/blob/main/modules/packages.nix"),
        "{info}"
    );
    assert!(
        info.contains("Documentation: https://flake.parts/options/flake-parts.html#opt-perSystem.packages"),
        "{info}"
    );
    let stats = call(&app, json!({"action": "stats", "source": "flake-parts"})).await;
    assert!(stats.contains("Total options: 1"), "{stats}");
}

#[tokio::test]
async fn empty_document_does_not_become_an_empty_catalogue() {
    let fixture = serve(|_| ok(r#"<main><h2 id="options">Options</h2></main>"#));
    let text = call(&fixture.app(), json!({"action": "stats", "source": "flake-parts"})).await;
    assert!(text.contains("Error (API_ERROR): Failed to parse flake-parts core options"), "{text}");
}
