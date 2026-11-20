use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::text::{collapse, ellipsize, error};

const LOCATE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_STDOUT_BYTES: u64 = 128 * 1024;
const MAX_STDERR_BYTES: u64 = 4 * 1024;
const MAX_QUERY_BYTES: usize = 4096;

struct LocalIndex {
    executable: PathBuf,
    explicit_db: Option<PathBuf>,
    provenance: String,
}

fn find_executable() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path).map(|dir| dir.join("nix-locate")).find(|candidate| {
        fs::metadata(candidate).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

fn default_database() -> Option<PathBuf> {
    let absolute = |key| env::var_os(key).map(PathBuf::from).filter(|path| path.is_absolute());
    let cache = absolute("XDG_CACHE_HOME").or_else(|| absolute("HOME").map(|home| home.join(".cache")))?;
    Some(cache.join("nix-index"))
}

fn wrapped_package(executable: &Path) -> Option<PathBuf> {
    let canonical = executable.canonicalize().ok()?;
    let package = canonical.strip_prefix("/nix/store").ok()?.components().next()?;
    let (_, name) = package.as_os_str().to_str()?.split_once('-')?;
    ["nix-index-with-full-db-", "nix-index-with-small-db-", "nix-index-with-db-"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        .then(|| Path::new("/nix/store").join(package))
}

fn describe_database(directory: &Path) -> String {
    let files = directory.join("files");
    let canonical = files.canonicalize().unwrap_or(files);
    if canonical.starts_with("/nix/store") {
        return format!("Database snapshot: {} (Nix store; revision unknown)", canonical.display());
    }
    let modified = fs::metadata(&canonical).and_then(|meta| meta.modified());
    match modified {
        Ok(time) => {
            let timestamp = DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Secs, true);
            format!("Database file: {} (modified {timestamp}; revision unknown)", canonical.display())
        }
        Err(_) => format!("Database file: {} (timestamp and revision unknown)", canonical.display()),
    }
}

impl LocalIndex {
    fn detect() -> Option<Self> {
        let executable = find_executable()?;
        let explicit_db = env::var_os("NIX_INDEX_DATABASE").map(PathBuf::from);
        let provenance = if let Some(database) = &explicit_db {
            describe_database(database)
        } else if let Some(package) = wrapped_package(&executable) {
            format!("Database snapshot: {} (nix-index-database wrapper; revision unknown)", package.display())
        } else if let Some(database) = default_database() {
            describe_database(&database)
        } else {
            "Database snapshot: unknown".to_owned()
        };
        Some(Self { executable, explicit_db, provenance })
    }

    fn command(&self, query: &str) -> Command {
        let mut command = Command::new(&self.executable);
        command.args(["--whole-name", "--color", "never", "--type", "r", "--type", "x", "--type", "s"]);
        if let Some(database) = &self.explicit_db {
            command.arg("--db").arg(database);
        }
        if query.starts_with('/') {
            command.arg("--at-root").arg("--").arg(query);
        } else {
            command.arg("--").arg(format!("/{query}"));
        }
        command
    }
}

async fn read_matches(stdout: impl AsyncRead + Unpin, limit: usize) -> io::Result<(Vec<String>, bool)> {
    let mut reader = BufReader::new(stdout.take(MAX_STDOUT_BYTES));
    let mut matches = Vec::new();
    let mut line = Vec::new();
    loop {
        if reader.read_until(b'\n', &mut line).await? == 0 {
            return Ok((matches, reader.get_ref().limit() == 0));
        }
        if line.last() != Some(&b'\n') && reader.get_ref().limit() == 0 {
            return Ok((matches, true));
        }
        let text = String::from_utf8_lossy(&line);
        if !text.trim().is_empty() {
            if matches.len() == limit {
                return Ok((matches, true));
            }
            matches.push(text.trim_end().chars().filter(|c| !c.is_control()).collect());
        }
        line.clear();
    }
}

async fn run(mut command: Command, limit: usize) -> Result<(Vec<String>, bool), String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|failure| failure.to_string())?;
    let stdout = child.stdout.take().ok_or("Could not open nix-locate stdout")?;
    let mut stderr = child.stderr.take().ok_or("Could not open nix-locate stderr")?;
    let results = async move {
        let (matches, truncated) = read_matches(stdout, limit).await?;
        let status = if truncated { None } else { Some(child.wait().await?) };
        Ok::<_, io::Error>((matches, truncated, status))
    };
    let diagnostic = async move {
        let mut bytes = Vec::new();
        (&mut stderr).take(MAX_STDERR_BYTES).read_to_end(&mut bytes).await?;
        tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await?;
        Ok::<_, io::Error>(bytes)
    };
    let ((matches, truncated, status), diagnostic) =
        tokio::try_join!(results, diagnostic).map_err(|failure| failure.to_string())?;
    if let Some(status) = status.filter(|status| !status.success()) {
        let message = collapse(&String::from_utf8_lossy(&diagnostic));
        return Err(format!("nix-locate failed ({status}): {}", ellipsize(&message, 1000)));
    }
    Ok((matches, truncated))
}

pub async fn search(query: &str, limit: usize) -> String {
    if query.is_empty() || query.ends_with('/') || query.len() > MAX_QUERY_BYTES || query.chars().any(char::is_control)
    {
        return error("Expected a literal file name or path (up to 4096 bytes)", "INVALID_INPUT");
    }
    let index = match tokio::task::spawn_blocking(LocalIndex::detect).await {
        Ok(Some(index)) => index,
        Ok(None) => {
            return error(
                "nix-locate was not found on PATH. Install nix-index and prepare a local database.",
                "UNAVAILABLE",
            );
        }
        Err(failure) => return error(failure, "LOCATE_ERROR"),
    };
    let (matches, truncated) = match tokio::time::timeout(LOCATE_TIMEOUT, run(index.command(query), limit)).await {
        Ok(Ok(result)) => result,
        Ok(Err(failure)) => return error(failure, "LOCATE_ERROR"),
        Err(_) => return error("nix-locate timed out", "LOCATE_ERROR"),
    };
    let title = if matches.is_empty() && !truncated {
        format!("No indexed files matched '{query}'")
    } else {
        format!("Indexed files matching '{query}' ({} shown):", matches.len())
    };
    let mut lines = vec![title, index.provenance];
    if !matches.is_empty() {
        lines.push(String::new());
        lines.extend(matches);
    }
    if truncated {
        lines.push("Output limit reached.".to_owned());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_uses_literal_path_components_and_honors_database_override() {
        let index = LocalIndex {
            executable: "nix-locate".into(),
            explicit_db: Some("/test/index".into()),
            provenance: String::new(),
        };
        let command = index.command("lib/X[11].so");
        let args: Vec<_> = command.as_std().get_args().collect();
        assert!(args.ends_with(&["--db".as_ref(), "/test/index".as_ref(), "--".as_ref(), "/lib/X[11].so".as_ref()]));
        assert!(!args.contains(&"--regex".as_ref()));
        assert!(!args.contains(&"--at-root".as_ref()));

        let command = index.command("/bin/--literal");
        let args: Vec<_> = command.as_std().get_args().collect();
        assert!(args.ends_with(&["--at-root".as_ref(), "--".as_ref(), "/bin/--literal".as_ref()]));
    }

    #[tokio::test]
    async fn output_bounds_keep_complete_results() -> io::Result<()> {
        let (matches, truncated) = read_matches(&b"first\nsecond\nthird\n"[..], 2).await?;
        assert_eq!(matches, ["first", "second"]);
        assert!(truncated);

        let content = format!("first\n{}", "x".repeat(MAX_STDOUT_BYTES as usize + 1));
        let (matches, truncated) = read_matches(content.as_bytes(), 2).await?;
        assert_eq!(matches, ["first"]);
        assert!(truncated);
        Ok(())
    }
}
