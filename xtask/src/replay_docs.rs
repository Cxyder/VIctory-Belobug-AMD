//! Check the replay option reference without building the Windows diagnostics binary.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use syn::punctuated::Punctuated;
use syn::{Expr, ExprLit, Item, Lit, Meta, Token};

fn declared(source: &str) -> Result<BTreeSet<String>> {
    let replay = syn::parse_file(source)?
        .items
        .into_iter()
        .find_map(|item| match item {
            Item::Struct(s) if s.ident == "Replay" => Some(s),
            _ => None,
        })
        .context("Replay argument struct not found")?;
    let mut names = BTreeSet::new();
    for field in replay.fields {
        for attr in field.attrs.iter().filter(|a| a.path().is_ident("arg")) {
            for meta in attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)? {
                if !meta.path().is_ident("long") {
                    continue;
                }
                let name = match meta {
                    Meta::Path(_) => field
                        .ident
                        .as_ref()
                        .context("unnamed long option")?
                        .to_string()
                        .trim_start_matches("r#")
                        .replace('_', "-"),
                    Meta::NameValue(n) => match n.value {
                        Expr::Lit(ExprLit {
                            lit: Lit::Str(s), ..
                        }) => s.value(),
                        _ => bail!("expected a string literal for #[arg(long = ...)]"),
                    },
                    _ => bail!("unsupported long option declaration"),
                };
                names.insert(format!("--{name}"));
            }
        }
    }
    ensure!(!names.is_empty(), "Replay has no long options");
    Ok(names)
}

fn documented(markdown: &str) -> Result<BTreeSet<String>> {
    // The recording/build instructions above this reference and the test instructions
    // below it contain Cargo flags rather than replay flags. lines() also handles CRLF.
    let mut lines = markdown.lines();
    ensure!(
        lines.any(|line| line == "## Running it"),
        "missing replay reference heading"
    );
    let mut names = BTreeSet::new();
    for line in lines {
        if line == "## Limits" {
            return Ok(names);
        }
        for (at, _) in line.match_indices("--") {
            let tail = &line[at + 2..];
            if !tail.starts_with(|c: char| c.is_ascii_alphanumeric())
                || line[..at].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                continue;
            }
            let len = tail
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-')
                .count();
            names.insert(format!("--{}", &tail[..len]));
        }
    }
    bail!("missing end of replay reference (## Limits)")
}

fn check(source: &str, markdown: &str) -> Result<()> {
    let args = declared(source)?;
    let docs = documented(markdown)?;
    let diff = |a: &BTreeSet<String>, b: &BTreeSet<String>| {
        a.difference(b).cloned().collect::<Vec<_>>().join(", ")
    };
    let missing = diff(&args, &docs);
    let unknown = diff(&docs, &args);
    let mut errors = Vec::new();
    if !missing.is_empty() {
        errors.push(format!("missing from docs/replay.md: {missing}"));
    }
    if !unknown.is_empty() {
        errors.push(format!("not declared by Replay: {unknown}"));
    }
    ensure!(errors.is_empty(), "{}", errors.join("\n"));
    Ok(())
}

pub(super) fn run(root: &Path) -> Result<()> {
    let read =
        |path: &str| fs::read_to_string(root.join(path)).with_context(|| format!("reading {path}"));
    check(
        &read("apps/victory/src/replay.rs")?,
        &read("docs/replay.md")?,
    )?;
    println!("replay option reference matches the declared long options");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
        struct Unrelated { #[arg(long)] ignore_me: bool }
        struct Replay {
            folder: String,
            #[arg(long, num_args = 0..=1)]
            flow_slots: usize,
            #[arg(long = "show-image")]
            image: bool,
        }"#;

    #[test]
    fn checks_both_directions_without_counting_build_or_test_options() {
        let doc = "build --profile diag\r\n## Running it\r\n|---|---|\r\n[--flow-slots=20] `--looks`\r\n## Limits\r\ntest --ignored";
        assert_eq!(
            check(SOURCE, doc).unwrap_err().to_string(),
            "missing from docs/replay.md: --show-image\nnot declared by Replay: --looks"
        );
        check(SOURCE, &doc.replace("--looks", "--show-image")).unwrap();
    }
}
