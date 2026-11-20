//! Local flake inputs, via `nix flake archive` and direct store reads.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::store::{Listing, ReadError, read_text, render_read, scan_directory, too_large, validate_store_path};
use crate::config::MAX_FILE_SIZE;
use crate::text::{error, format_size, str_of, strings, strip, truthy};

const NIX_TIMEOUT: Duration = Duration::from_secs(60);

fn nix_available() -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(dir.join("nix"))
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
    })
}

/// Run a nix command with a timeout. The child is killed when the future is
/// dropped, which covers both the timeout and client cancellation.
async fn run_nix(args: &[&str], cwd: &str) -> Result<String, String> {
    let child = Command::new("nix")
        .args(["--extra-experimental-features", "nix-command flakes"])
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(NIX_TIMEOUT, child).await {
        Err(_) => Err("Command timed out".to_owned()),
        Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => Err("nix command not found".to_owned()),
        Ok(Err(err)) => Err(err.to_string()),
        Ok(Ok(output)) if output.status.success() => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        Ok(Ok(output)) => Err(String::from_utf8_lossy(&output.stderr).into_owned()),
    }
}

/// Run `nix flake archive --json`.
async fn flake_data(flake_dir: &str) -> Result<Value, String> {
    if !Path::new(flake_dir).join("flake.nix").is_file() {
        return Err(format!("Not a flake directory: {flake_dir} (no flake.nix found)"));
    }
    let stdout =
        run_nix(&["flake", "archive", "--json", "--no-write-lock-file"], flake_dir).await.map_err(|stderr| {
            if stderr.to_lowercase().contains("experimental feature") {
                "Flakes not enabled. Enable with: nix-command flakes experimental features".to_owned()
            } else if stderr.contains("does not provide attribute") {
                format!("Invalid flake: {}", strip(&stderr))
            } else {
                format!("Failed to get flake inputs: {}", strip(&stderr))
            }
        })?;
    serde_json::from_str(&stdout).map_err(|err| format!("Failed to parse flake archive output: {err}"))
}

#[derive(Deserialize)]
struct LockFile {
    root: String,
    nodes: BTreeMap<String, LockNode>,
}

#[derive(Deserialize)]
struct LockNode {
    #[serde(default)]
    inputs: BTreeMap<String, LockInput>,
    locked: Option<Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LockInput {
    Node(String),
    Follows(Vec<String>),
}

impl LockFile {
    fn resolve_from_root<'a>(
        &'a self,
        path: &[&str],
        active_follows: &mut HashSet<(&'a str, &'a str)>,
    ) -> Option<(Option<&'a LockInput>, &'a str)> {
        let mut node_id = self.root.as_str();
        let mut last_input = None;
        for name in path {
            let (key, input) = self.nodes.get(node_id)?.inputs.get_key_value(*name)?;
            last_input = Some(input);
            match input {
                LockInput::Node(target) => node_id = target,
                LockInput::Follows(target) => {
                    let edge = (node_id, key.as_str());
                    if active_follows.len() >= 128 || !active_follows.insert(edge) {
                        return None;
                    }
                    let target: Vec<&str> = target.iter().map(String::as_str).collect();
                    let followed_id = self.resolve_from_root(&target, active_follows);
                    active_follows.remove(&edge);
                    node_id = followed_id?.1;
                }
            }
        }
        Some((last_input, node_id))
    }

    fn lookup(&self, name: &str) -> Option<(&LockInput, &LockNode)> {
        let root = self.nodes.get(&self.root)?;
        let path: Vec<&str> = if root.inputs.contains_key(name) {
            vec![name]
        } else if name.contains('/') {
            name.split('/').collect()
        } else {
            name.split('.').collect()
        };
        let (input, node_id) = self.resolve_from_root(&path, &mut HashSet::new())?;
        Some((input?, self.nodes.get(node_id)?))
    }
}

async fn load_lock(flake_dir: &str) -> Result<LockFile, String> {
    let path = Path::new(flake_dir).join("flake.lock");
    tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        File::open(&path)
            .and_then(|file| file.take(MAX_FILE_SIZE + 1).read_to_end(&mut bytes))
            .map_err(|failure| error(format!("Cannot read {}: {failure}", path.display()), "FLAKE_ERROR"))?;
        if bytes.len() as u64 > MAX_FILE_SIZE {
            return Err(too_large(bytes.len() as u64));
        }
        let lock: LockFile = serde_json::from_slice(&bytes)
            .map_err(|failure| error(format!("Invalid {}: {failure}", path.display()), "FLAKE_ERROR"))?;
        if !lock.nodes.contains_key(&lock.root) {
            return Err(error("Missing root node in flake.lock", "FLAKE_ERROR"));
        }
        Ok(lock)
    })
    .await
    .map_err(|failure| error(failure, "FLAKE_ERROR"))?
}

fn pin_details(locked: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let owner = str_of(locked, "owner");
    let repo = str_of(locked, "repo");
    let mut source = str_of(locked, "url");
    if source.is_empty() {
        source = str_of(locked, "path");
    }
    if !owner.is_empty() && !repo.is_empty() {
        lines.push(format!("Source: {owner}/{repo}"));
    } else if !source.is_empty() {
        lines.push(format!("Source: {source}"));
    }
    for (label, key) in [("Type", "type"), ("Lock revision", "rev"), ("NAR hash", "narHash"), ("Subdirectory", "dir")] {
        let value = str_of(locked, key);
        if !value.is_empty() {
            lines.push(format!("{label}: {value}"));
        }
    }
    lines
}

fn format_lock_input(lock: &LockFile, name: &str) -> Result<String, String> {
    let Some((input, node)) = lock.lookup(name) else {
        return Err(error(format!("Input '{name}' could not be resolved in flake.lock"), "NOT_FOUND"));
    };
    let mut lines = vec![format!("Input: {name}")];
    if let LockInput::Follows(path) = input {
        let shown = if path.is_empty() { "/".to_owned() } else { path.join("/") };
        lines.push(format!("Follows: {shown}"));
    }
    if let Some(locked) = &node.locked {
        lines.extend(pin_details(locked));
    }
    let names: Vec<&str> = node.inputs.keys().map(String::as_str).collect();
    if !names.is_empty() {
        lines.push(format!("Inputs: {}", names.join(", ")));
    }
    Ok(lines.join("\n"))
}

/// Inspect the local lock without evaluating the flake or materializing its inputs.
pub async fn flake_inputs_lock(flake_dir: &str, query: &str, limit: usize) -> String {
    let lock = match load_lock(flake_dir).await {
        Ok(lock) => lock,
        Err(failure) => return failure,
    };
    if !query.is_empty() {
        return format_lock_input(&lock, query).unwrap_or_else(|failure| failure);
    }
    let root = &lock.nodes[&lock.root];
    let count = root.inputs.len();
    let mut lines = vec![format!("Locked flake inputs ({count} direct inputs)")];
    for name in root.inputs.keys().take(limit) {
        lines.push(format_lock_input(&lock, name).unwrap_or_else(|failure| failure));
    }
    if count > limit {
        lines.push(format!("... and {} more inputs", count - limit));
    }
    lines.join("\n\n")
}

const PINNED_PACKAGE: &str = r#"
{ locked, directory, attrPath, system }:
let
  tree = builtins.fetchTree locked;
  source = tree.outPath + directory;
  pkgs = import source { inherit system; };
  select = value: name: if builtins.isAttrs value && builtins.hasAttr name value
    then builtins.getAttr name value else null;
  pkg = builtins.foldl' select pkgs attrPath;
  meta = pkg.meta or {};
  strings = value: builtins.filter builtins.isString
    (if builtins.isList value then value else [ value ]);
  licenseNames = value:
    if builtins.isList value then builtins.concatMap licenseNames value
    else if builtins.isAttrs value then strings (value.spdxId or (value.shortName or (value.fullName or null)))
    else strings value;
in
if pkg == null then null
else if !builtins.isAttrs pkg || (pkg.type or null) != "derivation" then { isPackage = false; }
else {
  isPackage = true;
  pname = pkg.pname or (builtins.parseDrvName pkg.name).name;
  version = pkg.version or (builtins.parseDrvName pkg.name).version;
  description = meta.description or null;
  homepage = strings (meta.homepage or null);
  mainProgram = meta.mainProgram or null;
  outputs = pkg.outputs or [];
  platforms = strings (meta.platforms or []);
  license = licenseNames (meta.license or null);
  sourceRevision = if ((builtins.readDir source).".git-revision" or null) == "regular"
    then builtins.readFile (source + "/.git-revision") else null;
  available = if pkgs ? lib.meta.availableOn && pkgs ? stdenv.hostPlatform
    then pkgs.lib.meta.availableOn pkgs.stdenv.hostPlatform pkg else null;
}
"#;

fn is_full_revision(revision: &str) -> bool {
    revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn pinned_expression(locked: &Value, attr: &str, system: &str) -> Result<String, String> {
    let unsupported = || error("Expected a locked nixpkgs-compatible source with an immutable pin", "FLAKE_ERROR");
    let mut fetch_attrs = locked.as_object().cloned().ok_or_else(unsupported)?;
    let kind = str_of(locked, "type");
    let fixed_revision = is_full_revision(str_of(locked, "rev"));
    if kind == "path" && !Path::new(str_of(locked, "path")).is_absolute() {
        return Err(error("Relative path inputs require their parent source to resolve", "FLAKE_ERROR"));
    }
    if kind.is_empty() || kind == "indirect" || (str_of(locked, "narHash").is_empty() && !fixed_revision) {
        return Err(unsupported());
    }
    let directory = fetch_attrs.remove("dir").unwrap_or(Value::Null);
    let directory = match directory.as_str() {
        Some(dir) if !dir.is_empty() => {
            if !Path::new(dir).components().all(|part| matches!(part, Component::Normal(_))) {
                return Err(error("Invalid dir in locked flake input", "FLAKE_ERROR"));
            }
            format!("/{dir}")
        }
        _ => String::new(),
    };
    let attr_path: Vec<&str> = attr.split('.').collect();
    if attr_path.iter().any(|part| part.is_empty()) {
        return Err(error(
            "Expected an exact nixpkgs attribute such as hello or python3Packages.requests",
            "INVALID_FORMAT",
        ));
    }
    if !system.contains('-') || !system.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(error("System required for pinned info (e.g., x86_64-linux or aarch64-darwin)", "INVALID_FORMAT"));
    }
    let args = json!({"locked": fetch_attrs, "directory": directory, "attrPath": attr_path, "system": system});
    let args = Value::String(args.to_string()).to_string().replace("${", "\\${");
    Ok(format!("({PINNED_PACKAGE}) (builtins.fromJSON {args})"))
}

/// Evaluate an exact package attribute from a nixpkgs-compatible locked input.
pub async fn pinned_info(flake_dir: &str, input: &str, attr: &str, system: &str) -> String {
    let lock = match load_lock(flake_dir).await {
        Ok(lock) => lock,
        Err(failure) => return failure,
    };
    let Some((_, node)) = lock.lookup(input) else {
        return error(format!("Input '{input}' could not be resolved in flake.lock"), "NOT_FOUND");
    };
    let Some(locked) = &node.locked else {
        return error(format!("Input '{input}' has no locked source in flake.lock"), "FLAKE_ERROR");
    };
    let expression = match pinned_expression(locked, attr, system) {
        Ok(expression) => expression,
        Err(failure) => return failure,
    };
    if !nix_available() {
        return nix_missing();
    }
    let output = run_nix(
        &[
            "eval",
            "--json",
            "--no-write-lock-file",
            "--option",
            "allow-import-from-derivation",
            "false",
            "--expr",
            &expression,
        ],
        flake_dir,
    )
    .await;
    let data: Value = match output {
        Ok(output) => match serde_json::from_str(&output) {
            Ok(data) => data,
            Err(failure) => return error(format!("Invalid package data from nix eval: {failure}"), "FLAKE_ERROR"),
        },
        Err(failure) => {
            return error(
                format!("Could not evaluate pinned nixpkgs input '{input}': {}", strip(&failure)),
                "FLAKE_ERROR",
            );
        }
    };
    if data.is_null() {
        return error(format!("Attribute '{attr}' not found in pinned input '{input}' for {system}"), "NOT_FOUND");
    }
    if data.get("isPackage").and_then(Value::as_bool) != Some(true) {
        return error(format!("Attribute '{attr}' in pinned input '{input}' is not a package"), "NOT_FOUND");
    }
    format_pinned_package(&data, locked, attr, input, system)
}

fn format_pinned_package(data: &Value, locked: &Value, attr: &str, input: &str, system: &str) -> String {
    let source_revision = strip(str_of(data, "sourceRevision"));
    let lock_revision = str_of(locked, "rev");
    let mut lines = Vec::new();
    if is_full_revision(source_revision)
        && is_full_revision(lock_revision)
        && !source_revision.eq_ignore_ascii_case(lock_revision)
    {
        lines.push(
            "WARNING: The evaluated source revision differs from the revision declared in flake.lock.".to_owned(),
        );
        lines.push(String::new());
    }
    lines.extend([
        format!("Package: {}", str_of(data, "pname")),
        format!("Attribute: {attr}"),
        format!("Version: {}", str_of(data, "version")),
        format!("Input: {input}"),
        format!("System: {system}"),
    ]);
    lines.extend(pin_details(locked));
    if is_full_revision(source_revision) {
        lines.push(format!("Source revision (.git-revision): {source_revision}"));
    }
    if let Some(available) = data.get("available").and_then(Value::as_bool) {
        lines.push(format!("Available on this system: {}", if available { "yes" } else { "no" }));
    }
    for (label, key) in [("Description", "description"), ("Main program", "mainProgram")] {
        let value = str_of(data, key);
        if !value.is_empty() {
            lines.push(format!("{label}: {value}"));
        }
    }
    for (label, key) in
        [("Homepage", "homepage"), ("Outputs", "outputs"), ("Platforms", "platforms"), ("License", "license")]
    {
        let values = strings(data.get(key));
        if !values.is_empty() {
            lines.push(format!("{label}: {}", values.join(", ")));
        }
    }
    lines.join("\n")
}

/// Flatten nested inputs: `flake-parts.nixpkgs-lib` -> store path.
pub fn flatten_inputs(data: &Value, prefix: &str, result: &mut BTreeMap<String, String>) {
    let Some(inputs) = data.get("inputs").and_then(Value::as_object) else {
        return;
    };
    for (name, info) in inputs {
        let full_name = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        let store_path = str_of(info, "path");
        if !store_path.is_empty() {
            result.insert(full_name.clone(), store_path.to_owned());
        }
        if info.get("inputs").is_some_and(truthy) {
            flatten_inputs(info, &full_name, result);
        }
    }
}

async fn load_inputs(flake_dir: &str) -> Result<BTreeMap<String, String>, String> {
    let data = flake_data(flake_dir).await.map_err(|message| error(message, "FLAKE_ERROR"))?;
    let mut inputs = BTreeMap::new();
    flatten_inputs(&data, "", &mut inputs);
    Ok(inputs)
}

fn missing_input(name: &str, inputs: &BTreeMap<String, String>) -> String {
    let available: Vec<&str> = inputs.keys().take(10).map(String::as_str).collect();
    let more = if inputs.len() > 10 { format!(" ... and {} more", inputs.len() - 10) } else { String::new() };
    error(format!("Input '{name}' not found. Available: {}{more}", available.join(", ")), "NOT_FOUND")
}

fn nix_missing() -> String {
    error("Nix is not installed or not in PATH", "NIX_NOT_FOUND")
}

/// List all flake inputs with their store paths.
pub async fn flake_inputs_list(flake_dir: &str) -> String {
    if !nix_available() {
        return nix_missing();
    }
    let data = match flake_data(flake_dir).await {
        Ok(data) => data,
        Err(message) => return error(message, "FLAKE_ERROR"),
    };
    let mut inputs = BTreeMap::new();
    flatten_inputs(&data, "", &mut inputs);
    if inputs.is_empty() {
        return "No inputs found for this flake.".to_owned();
    }
    let path = data.get("path").and_then(Value::as_str).unwrap_or(flake_dir);
    let mut lines =
        vec![format!("Flake inputs ({} found):", inputs.len()), format!("Flake path: {path}"), String::new()];
    for (name, store_path) in &inputs {
        lines.push(format!("* {name}"));
        lines.push(format!("  {store_path}"));
        lines.push(String::new());
    }
    strip(&lines.join("\n")).to_owned()
}

fn ls_target(store_path: &str, input_name: &str, subpath: &str) -> String {
    let target = Path::new(store_path).join(subpath);
    let target_str = target.to_string_lossy();
    if !validate_store_path(&target_str) {
        return error("Invalid path: must stay within /nix/store/", "SECURITY_ERROR");
    }
    let shown = if subpath.is_empty() { "/" } else { subpath };
    if !target.exists() {
        return error(format!("Path not found: {shown} in {input_name}"), "NOT_FOUND");
    }
    if !target.is_dir() {
        return error(format!("Not a directory: {shown} in {input_name}"), "NOT_DIRECTORY");
    }
    let listing = match scan_directory(&target) {
        Ok(listing) => listing,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return error(format!("Permission denied: {shown}"), "PERMISSION_ERROR");
        }
        Err(err) => return error(format!("Cannot list directory: {err}"), "OS_ERROR"),
    };
    if listing.is_empty() {
        return format!("Directory '{shown}' in {input_name} is empty.");
    }
    let display_path = if subpath.is_empty() { input_name.to_owned() } else { format!("{input_name}:{subpath}") };
    let mut lines = vec![
        format!("Contents of {display_path} ({} dirs, {} files):", listing.dirs.len(), listing.files.len()),
        String::new(),
    ];
    lines.extend(listing.dirs.iter().map(|name| format!("  {name}/")));
    lines.extend(listing.files.iter().map(|(name, size)| Listing::file_line(name, *size)));
    lines.join("\n")
}

/// List directory contents within a flake input (`input` or `input:subpath`).
pub async fn flake_inputs_ls(flake_dir: &str, query: &str) -> String {
    if !nix_available() {
        return nix_missing();
    }
    let (input_name, subpath) = query.split_once(':').unwrap_or((query, ""));
    let subpath = subpath.trim_start_matches('/');
    let inputs = match load_inputs(flake_dir).await {
        Ok(inputs) => inputs,
        Err(message) => return message,
    };
    let Some(store_path) = inputs.get(input_name) else {
        return missing_input(input_name, &inputs);
    };
    let (store_path, input_name, subpath) = (store_path.clone(), input_name.to_owned(), subpath.to_owned());
    tokio::task::spawn_blocking(move || ls_target(&store_path, &input_name, &subpath))
        .await
        .unwrap_or_else(|failure| error(failure, "OS_ERROR"))
}

fn read_target(store_path: &str, input_name: &str, file_path: &str, limit: usize) -> String {
    let target = Path::new(store_path).join(file_path);
    if !validate_store_path(&target.to_string_lossy()) {
        return error("Invalid path: must stay within /nix/store/", "SECURITY_ERROR");
    }
    if !target.exists() {
        return error(format!("File not found: {file_path} in {input_name}"), "NOT_FOUND");
    }
    if target.is_dir() {
        return error(format!("'{file_path}' is a directory. Use type='ls' to list contents."), "IS_DIRECTORY");
    }
    match read_text(&target) {
        Ok((size, text)) => render_read(&format!("{input_name}:{file_path}"), size, &text, limit),
        Err(ReadError::TooLarge(size)) => too_large(size),
        Err(ReadError::Binary(size)) => {
            error(format!("Binary file detected: {file_path} ({})", format_size(size)), "BINARY_FILE")
        }
        Err(ReadError::Access(err)) => error(format!("Cannot access file: {err}"), "OS_ERROR"),
        Err(ReadError::Read(err)) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            error(format!("Permission denied: {file_path}"), "PERMISSION_ERROR")
        }
        Err(ReadError::Read(err)) => error(format!("Cannot read file: {err}"), "OS_ERROR"),
    }
}

/// Read a file from a flake input (`input:path/to/file`).
pub async fn flake_inputs_read(flake_dir: &str, query: &str, limit: usize) -> String {
    if !nix_available() {
        return nix_missing();
    }
    let Some((input_name, file_path)) = query.split_once(':') else {
        return error("Read requires 'input:path' format (e.g., 'nixpkgs:flake.nix')", "INVALID_FORMAT");
    };
    let file_path = file_path.trim_start_matches('/');
    if file_path.is_empty() {
        return error("File path required (e.g., 'nixpkgs:flake.nix')", "INVALID_FORMAT");
    }
    let inputs = match load_inputs(flake_dir).await {
        Ok(inputs) => inputs,
        Err(message) => return message,
    };
    let Some(store_path) = inputs.get(input_name) else {
        return missing_input(input_name, &inputs);
    };
    let (store_path, input_name, file_path) = (store_path.clone(), input_name.to_owned(), file_path.to_owned());
    tokio::task::spawn_blocking(move || read_target(&store_path, &input_name, &file_path, limit))
        .await
        .unwrap_or_else(|failure| error(failure, "OS_ERROR"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lock_fixture() -> Result<LockFile, serde_json::Error> {
        serde_json::from_value(json!({"root": "root", "nodes": {
            "root": {"inputs": {"pkgs": "nixpkgs", "parts": "parts", "alias": ["parts", "nixpkgs"], "cycle": ["cycle"]}},
            "parts": {"inputs": {"nixpkgs": ["pkgs"]}, "locked": {"type": "github"}},
            "nixpkgs": {"locked": {"type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "7".repeat(40)}}
        }}))
    }

    #[test]
    fn lock_follows_resolution_uses_root_paths() -> Result<(), String> {
        let lock = lock_fixture().map_err(|failure| failure.to_string())?;
        for key in ["parts/nixpkgs", "parts.nixpkgs", "alias"] {
            let (_, node) = lock.lookup(key).ok_or_else(|| format!("missing input {key}"))?;
            let locked = node.locked.as_ref().ok_or_else(|| format!("missing pin for {key}"))?;
            assert_eq!(str_of(locked, "rev"), "7".repeat(40));
        }
        assert!(lock.lookup("cycle").is_none());
        assert!(lock.lookup("parts/missing").is_none());
        assert!(format_lock_input(&lock, "alias")?.contains("Follows: parts/nixpkgs"));
        Ok(())
    }

    #[test]
    fn pinned_expression_takes_the_locked_source_and_quotes_attr_as_data() -> Result<(), String> {
        let lock = lock_fixture().map_err(|failure| failure.to_string())?;
        let (_, node) = lock.lookup("alias").ok_or("missing alias")?;
        let locked = node.locked.as_ref().ok_or("missing alias pin")?;
        let expression = pinned_expression(locked, "python3Packages.${literal}", "aarch64-linux")?;
        assert!(expression.contains(r#"\"attrPath\":[\"python3Packages\",\"\${literal}\"]"#));
        assert!(expression.contains(&"7".repeat(40)));
        assert!(expression.contains(r#"\"system\":\"aarch64-linux\""#));
        assert!(pinned_expression(locked, "python3Packages.", "aarch64-linux").is_err());
        assert!(pinned_expression(&json!({"type": "github", "ref": "master"}), "hello", "x86_64-linux").is_err());
        Ok(())
    }

    #[test]
    fn pinned_package_flags_a_different_full_source_revision_with_the_same_prefix() {
        let lock_rev = format!("0123456789ab{}", "c".repeat(28));
        let source_rev = format!("0123456789ab{}", "d".repeat(28));
        let locked = json!({"type":"tarball", "rev":lock_rev});
        let data = json!({"pname":"hello", "version":"1.0", "sourceRevision":format!("{source_rev}\n")});
        let text = format_pinned_package(&data, &locked, "hello", "nixpkgs", "x86_64-linux");
        assert!(text.starts_with("WARNING: The evaluated source revision differs"), "{text}");
        assert!(text.contains(&format!("Lock revision: {lock_rev}\n")), "{text}");
        assert!(text.contains(&format!("Source revision (.git-revision): {source_rev}")), "{text}");

        let data = json!({"pname":"hello", "version":"1.0", "sourceRevision":"0123456789ab"});
        let text = format_pinned_package(&data, &locked, "hello", "nixpkgs", "x86_64-linux");
        assert!(!text.contains("WARNING") && !text.contains("Source revision"), "{text}");
    }

    #[test]
    fn nested_inputs_flatten_with_dotted_names() {
        let data = json!({"inputs": {
            "nixpkgs": {"path": "/nix/store/a"},
            "parts": {"path": "/nix/store/b", "inputs": {"lib": {"path": "/nix/store/c", "inputs": {}}}},
            "empty": {"path": ""}
        }});
        let mut inputs = BTreeMap::new();
        flatten_inputs(&data, "", &mut inputs);
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs["parts.lib"], "/nix/store/c");
    }

    #[test]
    fn missing_input_lists_ten_then_counts() {
        let inputs: BTreeMap<String, String> = (0..12).map(|i| (format!("i{i:02}"), String::new())).collect();
        assert!(missing_input("x", &inputs).ends_with("i09 ... and 2 more"));
    }

    #[test]
    fn traversal_out_of_an_input_is_refused() {
        let message = ls_target("/nix/store/abc-x", "x", "../../../etc");
        assert_eq!(message, "Error (SECURITY_ERROR): Invalid path: must stay within /nix/store/");
    }
}
