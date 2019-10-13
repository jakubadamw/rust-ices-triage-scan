#![feature(generators)]
#![feature(proc_macro_hygiene)]
#![feature(stmt_expr_attributes)]

#[macro_use]
extern crate lazy_static;

use futures_async_stream::{async_try_stream, for_await};
use serde::Deserialize;

const ISSUES_URL: &str =
    "https://api.github.com/repos/rust-lang/rust/issues?labels=I-ice&state=open";

#[derive(Debug)]
enum Error {
    Io(std::io::Error),
    PathPersist(tempfile::PathPersistError),
    Persist(tempfile::PersistError),
    Surf(surf::Exception),
}

impl std::convert::From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::convert::From<tempfile::PathPersistError> for Error {
    fn from(e: tempfile::PathPersistError) -> Self {
        Self::PathPersist(e)
    }
}

impl std::convert::From<tempfile::PersistError> for Error {
    fn from(e: tempfile::PersistError) -> Self {
        Self::Persist(e)
    }
}

impl std::convert::From<surf::Exception> for Error {
    fn from(e: surf::Exception) -> Self {
        Self::Surf(e)
    }
}

#[derive(Debug, Deserialize)]
struct Issue {
    html_url: String,
    body: String,
}

lazy_static! {
    static ref CODEBLOCK_REGEXP: regex::Regex =
        regex::Regex::new(r"```rust(?P<snippet>[^`]+)```").unwrap();
}

fn get_mcves<'i>(issue: &'i Issue) -> impl Iterator<Item = String> + 'i {
    CODEBLOCK_REGEXP
        .captures_iter(&issue.body)
        .map(|c| c["snippet"].trim().to_owned())
}

fn get_next_link(response: &surf::Response) -> Option<String> {
    response
        .header("Link")
        .and_then(|value| {
            value
                .split(',')
                .map(str::trim)
                .map(|link_str| {
                    let mut parts = link_str.split(';').map(str::trim);

                    let mut url = parts.next().unwrap();
                    assert!(url.starts_with('<'));
                    assert!(url.ends_with('>'));
                    url = &url[1..url.len() - 1];

                    let mut rel = parts.next().unwrap();
                    assert!(rel.starts_with("rel=\""));
                    assert!(rel.ends_with('"'));
                    rel = &rel[5..rel.len() - 1];

                    (rel, url)
                })
                .find(|(rel, _)| *rel == "next")
        })
        .map(|(_, url)| url.to_owned())
}

#[async_try_stream(ok = Issue, error = Error)]
async fn get_issues() {
    let mut next_url = Some(ISSUES_URL.to_owned());

    while let Some(url) = next_url {
        let mut response = surf::get(url).await?;
        let issues: Vec<Issue> = response.body_json().await?;
        for issue in issues {
            yield issue;
        }
        next_url = get_next_link(&response);
    }
}

#[derive(Clone, Debug, PartialEq)]
enum CompilationResult {
    ICE,
    Failed,
    Compiled,
}

impl std::fmt::Display for CompilationResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use colored::*;
        match *self {
            Self::Compiled => write!(f, "{:20}", "compiled".green()),
            Self::Failed => write!(f, "{:20}", "failed".yellow()),
            Self::ICE => write!(f, "{:20}", "ICE".red()),
        }
    }
}

async fn run_test(toolchain: &str, input: &str) -> Result<CompilationResult, Error> {
    use tokio::net::process::Command;

    let (stdin, stdin_path) = tempfile::NamedTempFile::new()?.keep()?;
    std::fs::write(&stdin_path, input)?;

    let artifact_path = tempfile::NamedTempFile::new()?.into_temp_path().keep()?;

    let (stdout, stdout_path) = tempfile::NamedTempFile::new()?.keep()?;
    let stderr = stdout.try_clone()?;

    let output = Command::new("rustup")
        .arg("run")
        .arg(toolchain)
        .arg("rustc")
        .arg("-")
        .arg("-o")
        .arg(&artifact_path)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()?
        .wait_with_output()
        .await?;

    let result = if output.status.success() {
        CompilationResult::Compiled
    } else {
        let buffer = std::fs::read_to_string(&stdout_path)?;
        if buffer.contains("internal compiler error") {
            CompilationResult::ICE
        } else {
            CompilationResult::Failed
        }
    };

    Ok(result)
}

const TOOLCHAINS: &[&str] = &[
    "nightly-2019-02-01",
    "nightly-2019-09-01",
    "nightly-2019-10-01",
    "nightly",
];

fn print_row(html_url: &str, results: Vec<CompilationResult>) {
    use colored::*;
    use prettytable::{format, Cell, Row, Table};

    let changed = !results.windows(2).all(|w| w[0] == w[1]);
    let url = if changed {
        html_url.blue()
    } else {
        html_url.normal()
    };

    let row = std::iter::once(format!("{:50}", url))
        .chain(results.into_iter().map(|r| r.to_string()))
        .map(|s| Cell::new(&s))
        .collect();

    let mut table = Table::new();
    table.set_format(*format::consts::FORMAT_NO_BORDER_LINE_SEPARATOR);
    table.add_row(Row::new(row));
    table.printstd();
}

fn print_headers() {
    use prettytable::{format, Cell, Row, Table};

    let row = std::iter::once(format!("{:49}", ""))
        .chain(
            TOOLCHAINS
                .iter()
                .map(|toolchain| format!("{:20}", toolchain)),
        )
        .map(|s| Cell::new(&s))
        .collect();

    let mut table = Table::new();
    table.set_format(*format::consts::FORMAT_NO_TITLE);
    table.add_row(Row::new(row));
    table.printstd();
}

#[tokio::main]
async fn main() {
    use futures::stream::StreamExt;

    let mut i: u16 = 0;

    let mut issues_without_mcve = vec![];

    #[for_await]
    for issue in get_issues() {
        let issue = issue.unwrap();
        let mcve = get_mcves(&issue).next();

        if let Some(mcve) = mcve {
            let stream = TOOLCHAINS
                .iter()
                .enumerate()
                .map(|(index, toolchain)| {
                    let mcve = &mcve;
                    async move { (index, run_test(toolchain, mcve).await) }
                })
                .collect::<futures::stream::FuturesUnordered<_>>();

            let mut results: Vec<(usize, Result<CompilationResult, Error>)> =
                stream.collect().await;
            results.sort_by_key(|el| el.0);
            let results = results.into_iter().map(|el| el.1.unwrap()).collect();

            if i % 10 == 0 {
                print_headers();
            }
            print_row(&issue.html_url, results);
            i += 1;
        } else {
            issues_without_mcve.push(issue);
        }
    }

    println!();
    println!("Issues without MCVEs:");
    for issue in issues_without_mcve {
        println!("{}", issue.html_url);
    }
}
