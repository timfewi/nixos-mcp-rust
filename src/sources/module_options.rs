use std::cmp::Reverse;
use std::sync::LazyLock;

use scraper::{ElementRef, Html, Selector};
use tokio::sync::OnceCell;

use crate::App;
use crate::html::{get_text, selector};
use crate::http::Http;
use crate::text::{casefold, collapse, commas, ellipsize, error, score_option_match, tally};

static OPTION_HEADINGS: LazyLock<Selector> = LazyLock::new(|| selector("main > h2[id]"));
static HEADING_ANCHOR: LazyLock<Selector> = LazyLock::new(|| selector("a.header"));
static METADATA_LABEL: LazyLock<Selector> = LazyLock::new(|| selector("p > em:first-child"));
static LINKS: LazyLock<Selector> = LazyLock::new(|| selector("a[href]"));

#[derive(Clone, Copy)]
pub enum ModuleSource {
    Microvm,
    FlakeParts,
}

impl ModuleSource {
    fn label(self) -> &'static str {
        match self {
            Self::Microvm => "MicroVM.nix",
            Self::FlakeParts => "flake-parts core",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModulePage {
    MicrovmHost,
    MicrovmGuest,
    FlakePartsCore,
}

impl ModulePage {
    fn url(self) -> &'static str {
        match self {
            Self::MicrovmHost => "https://microvm-nix.github.io/microvm.nix/host-options.html",
            Self::MicrovmGuest => "https://microvm-nix.github.io/microvm.nix/microvm-options.html",
            Self::FlakePartsCore => "https://flake.parts/options/flake-parts.html",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::MicrovmHost => "MicroVM.nix host",
            Self::MicrovmGuest => "MicroVM.nix guest",
            Self::FlakePartsCore => "flake-parts core",
        }
    }
}

struct ModuleOption {
    name: String,
    page: ModulePage,
    anchor: String,
    description: String,
    kind: String,
    default: String,
    example: String,
    declarations: Vec<String>,
}

enum Section {
    Description,
    Type,
    Default,
    Example,
    Declarations,
}

impl Section {
    fn from_label(label: &str) -> Option<Self> {
        match label {
            "Type:" => Some(Self::Type),
            "Default:" => Some(Self::Default),
            "Example:" => Some(Self::Example),
            "Declared by:" => Some(Self::Declarations),
            _ => None,
        }
    }
}

fn append_block(field: &mut String, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    if !field.is_empty() {
        field.push('\n');
    }
    field.push_str(value);
}

fn parse_option(heading: ElementRef<'_>, page: ModulePage) -> Option<ModuleOption> {
    let name = get_text(heading.select(&HEADING_ANCHOR).next()?, "", true);
    let anchor = heading.attr("id")?;
    let is_option = match page {
        ModulePage::MicrovmHost | ModulePage::MicrovmGuest => name.starts_with("microvm."),
        ModulePage::FlakePartsCore => anchor.starts_with("opt-"),
    };
    if name.is_empty() || !is_option {
        return None;
    }
    let mut option = ModuleOption {
        name,
        page,
        anchor: anchor.to_owned(),
        description: String::new(),
        kind: String::new(),
        default: String::new(),
        example: String::new(),
        declarations: Vec::new(),
    };
    let mut section = Section::Description;
    for block in heading.next_siblings().filter_map(ElementRef::wrap) {
        let tag = block.value().name();
        if matches!(tag, "h1" | "h2") {
            break;
        }
        let text = get_text(block, "", false);
        let mut text = text.trim();
        if tag == "p"
            && let Some(label) = block.select(&METADATA_LABEL).next()
            && let Some(next) = Section::from_label(&get_text(label, "", true))
        {
            let label = get_text(label, "", true);
            section = next;
            text = text.strip_prefix(&label).unwrap_or(text);
        }
        let value = if tag == "pre" { text.to_owned() } else { collapse(text) };
        match section {
            Section::Description => append_block(&mut option.description, &value),
            Section::Type => append_block(&mut option.kind, &value),
            Section::Default => append_block(&mut option.default, &value),
            Section::Example => append_block(&mut option.example, &value),
            Section::Declarations => option.declarations.extend(
                block
                    .select(&LINKS)
                    .filter_map(|link| link.attr("href"))
                    .filter(|url| url.starts_with("https://"))
                    .map(str::to_owned),
            ),
        }
    }
    Some(option)
}

fn parse_catalogue(html: &str, page: ModulePage) -> Vec<ModuleOption> {
    Html::parse_document(html).select(&OPTION_HEADINGS).filter_map(|heading| parse_option(heading, page)).collect()
}

async fn fetch_catalogue(http: &Http, page: ModulePage) -> Result<Vec<ModuleOption>, String> {
    let response = http.send(http.get(page.url()), 30).await.and_then(|response| response.ok());
    let response = response.map_err(|failure| format!("Failed to fetch {} options: {failure}", page.label()))?;
    let options = tokio::task::spawn_blocking(move || parse_catalogue(&response.text(), page))
        .await
        .map_err(|failure| format!("Failed to parse {} options: {failure}", page.label()))?;
    if options.is_empty() {
        return Err(format!("Failed to parse {} options: no options found at {}", page.label(), page.url()));
    }
    Ok(options)
}

#[derive(Default)]
pub(crate) struct ModuleOptionsCache {
    microvm: OnceCell<Vec<ModuleOption>>,
    flake_parts: OnceCell<Vec<ModuleOption>>,
}

impl ModuleOptionsCache {
    async fn load(&self, http: &Http, source: ModuleSource) -> Result<&[ModuleOption], String> {
        let cell = match source {
            ModuleSource::Microvm => &self.microvm,
            ModuleSource::FlakeParts => &self.flake_parts,
        };
        cell.get_or_try_init(|| async {
            let mut options = match source {
                ModuleSource::Microvm => {
                    let (mut host, guest) = tokio::try_join!(
                        fetch_catalogue(http, ModulePage::MicrovmHost),
                        fetch_catalogue(http, ModulePage::MicrovmGuest),
                    )?;
                    host.extend(guest);
                    host
                }
                ModuleSource::FlakeParts => fetch_catalogue(http, ModulePage::FlakePartsCore).await?,
            };
            options.sort_by_cached_key(|option| casefold(&option.name));
            Ok(options)
        })
        .await
        .map(Vec::as_slice)
    }
}

fn format_list<'a>(title: String, options: impl Iterator<Item = &'a ModuleOption>) -> String {
    let mut lines = vec![title];
    for option in options {
        lines.push(String::new());
        lines.push(format!("* {} ({})", option.name, option.page.label()));
        if !option.kind.is_empty() {
            lines.push(format!("  Type: {}", option.kind));
        }
        if !option.description.is_empty() {
            lines.push(format!("  {}", ellipsize(&collapse(&option.description), 200)));
        }
    }
    lines.join("\n")
}

pub async fn search(app: &App, source: ModuleSource, query: &str, limit: i64) -> String {
    let options = match app.caches.module_options.load(&app.http, source).await {
        Ok(options) => options,
        Err(failure) => return error(failure, "API_ERROR"),
    };
    let query = query.trim();
    let mut matches: Vec<_> = options
        .iter()
        .filter_map(|option| {
            let score = score_option_match(&option.name, &collapse(&option.description), query);
            (score > 0).then_some((score, option))
        })
        .collect();
    matches.sort_by_key(|(score, _)| Reverse(*score));
    matches.truncate(limit as usize);
    let label = source.label();
    if matches.is_empty() {
        return format!("No {label} options found matching '{query}'");
    }
    format_list(
        format!("Found {} {label} options matching '{query}':", matches.len()),
        matches.into_iter().map(|(_, option)| option),
    )
}

pub async fn info(app: &App, source: ModuleSource, name: &str) -> String {
    let options = match app.caches.module_options.load(&app.http, source).await {
        Ok(options) => options,
        Err(failure) => return error(failure, "API_ERROR"),
    };
    let name = name.trim();
    let folded = casefold(name);
    let exact = options.iter().find(|option| option.name == name);
    let Some(option) = exact.or_else(|| options.iter().find(|option| casefold(&option.name) == folded)) else {
        return error(format!("{} option '{name}' not found", source.label()), "NOT_FOUND");
    };
    let mut lines = vec![format!("Option: {}", option.name), format!("Module: {}", option.page.label())];
    for (label, value) in [
        ("Type", &option.kind),
        ("Description", &option.description),
        ("Default", &option.default),
        ("Example", &option.example),
    ] {
        if !value.is_empty() {
            lines.push(format!("{label}: {}", ellipsize(value, 4_000)));
        }
    }
    for declaration in &option.declarations {
        lines.push(format!("Declared in: {declaration}"));
    }
    lines.push(format!("Documentation: {}#{}", option.page.url(), option.anchor));
    lines.join("\n")
}

pub async fn stats(app: &App, source: ModuleSource) -> String {
    let options = match app.caches.module_options.load(&app.http, source).await {
        Ok(options) => options,
        Err(failure) => return error(failure, "API_ERROR"),
    };
    let label = source.label();
    let mut lines = vec![format!("{label} Statistics:"), format!("* Total options: {}", commas(options.len() as i64))];
    for (name, count) in tally(options.iter().map(|option| option.page.label())) {
        lines.push(format!("* {name}: {} options", commas(count)));
    }
    lines.join("\n")
}

pub async fn browse(app: &App, source: ModuleSource, prefix: &str, limit: i64) -> String {
    let options = match app.caches.module_options.load(&app.http, source).await {
        Ok(options) => options,
        Err(failure) => return error(failure, "API_ERROR"),
    };
    let label = source.label();
    let prefix = prefix.trim().trim_end_matches('.');
    if prefix.is_empty() {
        let mut lines = vec![format!("{label} option categories:")];
        lines.extend(
            tally(
                options.iter().map(|option| option.name.split_once('.').map_or(option.name.as_str(), |(root, _)| root)),
            )
            .into_iter()
            .take(limit as usize)
            .map(|(name, count)| format!("* {name} ({} options)", commas(count))),
        );
        return lines.join("\n");
    }
    let folded = casefold(prefix);
    let nested = format!("{folded}.");
    let matches: Vec<_> = options
        .iter()
        .filter(|option| {
            let name = casefold(&option.name);
            name == folded || name.starts_with(&nested)
        })
        .collect();
    if matches.is_empty() {
        return format!("No {label} options found with prefix '{prefix}'");
    }
    format_list(
        format!("{label} options with prefix '{prefix}' ({} found):", matches.len()),
        matches.into_iter().take(limit as usize),
    )
}
