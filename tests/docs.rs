//! The README says what the program does: its commands are the ones `--help`
//! prints, and its settings table is the table the program reads
//! (`config::SPECS`): every setting once, with its environment variable, its
//! default and its units. When one changes without the other, this fails.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use psp_mint::config::SPECS;

fn readme() -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md")).unwrap()
}

/// The text of `## heading`, up to the next `## `.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let start = text
        .find(&format!("\n## {heading}\n"))
        .unwrap_or_else(|| panic!("the README has no `## {heading}`"));
    let rest = &text[start + 1..];
    match rest[3..].find("\n## ") {
        Some(end) => &rest[..end + 3],
        None => rest,
    }
}

/// The rows of the table in `text`: `| a | b | c |` as its cells, without the
/// header and the rule under it. The last cell keeps any ` | ` it holds.
fn rows(text: &str, columns: usize) -> Vec<Vec<String>> {
    text.lines()
        .filter(|line| line.starts_with("| `"))
        .map(|line| {
            let inner = line.trim_start_matches("| ").trim_end_matches(" |");
            inner
                .splitn(columns, " | ")
                .map(|cell| cell.trim().to_string())
                .collect()
        })
        .collect()
}

fn unquote(cell: &str) -> &str {
    cell.trim_matches('`')
}

#[test]
fn the_commands_in_the_readme_are_the_commands_help_prints() {
    let out = Command::new(env!("CARGO_BIN_EXE_psp-mint"))
        .arg("--help")
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    let usage = String::from_utf8(out.stdout).unwrap();
    // the command lines of the usage: two spaces, then the command
    let printed: Vec<&str> = usage
        .lines()
        .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
        .map(|l| l.trim().split(' ').next().unwrap())
        .collect();
    assert_eq!(
        printed,
        ["serve", "run", "config", "pubkey", "encrypt", "--help"]
    );

    let text = readme();
    let table = section(&text, "Commands");
    let documented: Vec<String> = rows(table, 2)
        .into_iter()
        .map(|row| {
            // "`run` (the default)" and "`encrypt <a> <b>`": the command word
            let cell = row[0].trim_start_matches('`');
            cell.split(['`', ' ']).next().unwrap().to_string()
        })
        .collect();
    assert_eq!(documented, printed);
}

#[test]
fn the_settings_table_in_the_readme_is_the_table_the_program_reads() {
    let text = readme();
    let table = rows(section(&text, "Settings"), 5);
    assert_eq!(
        table.iter().map(|r| unquote(&r[0])).collect::<Vec<_>>(),
        SPECS.iter().map(|s| s.key).collect::<Vec<_>>(),
        "the README lists the settings in the order of the table in config.rs, each once"
    );
    for (row, spec) in table.iter().zip(SPECS) {
        assert_eq!(unquote(&row[1]), spec.env, "{}", spec.key);
        // the units start with the spec's own words
        let lead = spec.units.split([',', '(']).next().unwrap().trim();
        assert!(
            row[3].starts_with(lead),
            "{}: units `{}` do not start with `{lead}`",
            spec.key,
            row[3]
        );
        assert!(!row[4].is_empty(), "{} says nothing", spec.key);
        // The defaults that depend on the network (and the operating system)
        // are described, not copied: see the "Networks" section.
        let per_network = ["bitcoin.rpc_url", "bitcoin.wallet", "bitcoin.cookie"];
        if per_network.contains(&spec.key) {
            assert!(
                row[2].starts_with("per network"),
                "{}: {}",
                spec.key,
                row[2]
            );
        } else if spec.default.is_empty() {
            assert!(
                row[2].starts_with("empty") || row[2].starts_with("none"),
                "{}: {}",
                spec.key,
                row[2]
            );
        } else {
            assert!(
                row[2].contains(&format!("`{}`", spec.default)),
                "{}: default `{}` is not in `{}`",
                spec.key,
                spec.default,
                row[2]
            );
        }
    }
}
