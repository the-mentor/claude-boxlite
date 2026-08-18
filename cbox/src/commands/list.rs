//! `cbox list` — every box across every per-name home.
//!
//! Homes are per box name, so listing means walking them all. Columns follow
//! `boxlite`'s own CLI (ID, IMAGE, STATUS, CREATED) plus one this repo needs
//! that the SDK cannot supply: ORIGIN, the directory a box was created from.
//! Names are derived from the git root, so without that column there is
//! nothing on screen tying a box back to the checkout it belongs to. The box
//! matching the current directory is additionally marked, since the derived
//! default name is otherwise invisible.

use anyhow::{Context, Result};
use boxlite::{BoxStatus, BoxliteOptions, BoxliteRuntime};

use crate::{config, naming, sidecar};

const NAME_W: usize = 20;
const ID_W: usize = 9;
const STATUS_W: usize = 10;
const IMAGE_W: usize = 24;
const CREATED_W: usize = 16;
const ORIGIN_W: usize = 30;

pub async fn run(all: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read current directory")?;
    let here = naming::resolve(None, &cwd).name;

    let root = config::box_home("_").parent().unwrap().to_path_buf();
    let Ok(entries) = std::fs::read_dir(&root) else {
        println!("cbox: no boxes yet");
        return Ok(());
    };

    let mut found = false;
    let mut header_printed = false;

    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(String::from) else { continue };
        let home = entry.path();

        let runtime = match BoxliteRuntime::new(BoxliteOptions {
            home_dir: home.clone(),
            image_registries: vec![],
        }) {
            Ok(r) => r,
            // A home locked by a running `cbox up` cannot be opened here —
            // the common case being a second terminal listing while the
            // first is still attached. Report it and move on rather than
            // aborting the whole listing.
            Err(_) => {
                if !header_printed {
                    print_header();
                    header_printed = true;
                }
                println!("{} (in use)", cell(&name, NAME_W));
                found = true;
                continue;
            }
        };

        let origin = sidecar::read(&home).map(|s| s.origin).unwrap_or_default();

        for info in runtime.list_info().await.unwrap_or_default() {
            if !should_show(info.status, all) {
                continue;
            }
            if !header_printed {
                print_header();
                header_printed = true;
            }
            let row = Row {
                name: info.name.clone().unwrap_or_else(|| name.clone()),
                id: info.id.short().to_string(),
                status: info.status.to_string(),
                image: info.image.clone(),
                created: info.created_at.format("%Y-%m-%d %H:%M").to_string(),
                origin: origin.clone(),
                marker: marker_for(&name, &here).to_string(),
            };
            println!("{}", format_row(&row));
            found = true;
        }
    }

    if !found {
        println!("cbox: no boxes yet");
    }
    Ok(())
}

/// One rendered row's worth of already-stringified fields. Kept as owned
/// `String`s rather than borrowing from `BoxInfo` so `format_row` — the part
/// worth unit-testing — doesn't need a live runtime to call.
struct Row {
    name: String,
    id: String,
    status: String,
    image: String,
    created: String,
    origin: String,
    marker: String,
}

fn print_header() {
    let header = Row {
        name: "NAME".into(),
        id: "ID".into(),
        status: "STATUS".into(),
        image: "IMAGE".into(),
        created: "CREATED".into(),
        origin: "ORIGIN".into(),
        marker: String::new(),
    };
    println!("{}", format_row(&header));
}

/// Whether a box's status earns it a row. `all` overrides the default of
/// hiding stopped boxes, which would otherwise accumulate forever since
/// `cbox down` — not stopping — is what removes them.
fn should_show(status: BoxStatus, all: bool) -> bool {
    all || status != BoxStatus::Stopped
}

/// The current-directory marker: visible only on the one box whose derived
/// name matches where `cbox list` was run from.
fn marker_for(box_name: &str, here: &str) -> &'static str {
    if box_name == here { " <- here" } else { "" }
}

/// Fit a value into a fixed-width column: truncate (with a trailing `…` so
/// truncation is visible) if it's too long, pad if it's short. This is what
/// keeps a long image reference or origin path from wrapping the row and
/// destroying the alignment of every column after it.
fn cell(s: &str, width: usize) -> String {
    format!("{:<width$}", truncate(s, width), width = width)
}

fn truncate(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return "…".to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Name and origin are the two things a human scans for, so they bookend the
/// row: name leftmost (first thing read), origin last (right before the
/// marker a reader is also hunting for).
fn format_row(row: &Row) -> String {
    format!(
        "{} {} {} {} {} {}{}",
        cell(&row.name, NAME_W),
        cell(&row.id, ID_W),
        cell(&row.status, STATUS_W),
        cell(&row.image, IMAGE_W),
        cell(&row.created, CREATED_W),
        cell(&row.origin, ORIGIN_W),
        row.marker,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_leaves_short_strings_untouched() {
        assert_eq!(truncate("hi", 5), "hi");
        assert_eq!(truncate("exact", 5), "exact");
    }

    #[test]
    fn truncate_marks_long_strings_with_a_trailing_ellipsis_at_the_fixed_width() {
        let out = truncate("hello world", 5);
        assert_eq!(out, "hell…");
        assert_eq!(out.chars().count(), 5);
    }

    #[test]
    fn truncate_handles_pathological_widths_without_panicking() {
        assert_eq!(truncate("hello", 0), "");
        assert_eq!(truncate("hello", 1), "…");
    }

    #[test]
    fn cell_pads_short_values_to_the_column_width() {
        assert_eq!(cell("hi", 5), "hi   ");
    }

    #[test]
    fn cell_truncates_long_values_rather_than_widening_the_column() {
        let out = cell("a-very-long-image-reference:latest", 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'), "should be visibly truncated: {out:?}");
    }

    #[test]
    fn should_show_hides_stopped_boxes_unless_all_is_set() {
        assert!(!should_show(BoxStatus::Stopped, false));
        assert!(should_show(BoxStatus::Stopped, true));
        assert!(should_show(BoxStatus::Running, false));
        assert!(should_show(BoxStatus::Running, true));
    }

    #[test]
    fn marker_for_flags_only_the_box_matching_the_current_directory() {
        assert_eq!(marker_for("claude-boxlite", "claude-boxlite"), " <- here");
        assert_eq!(marker_for("other-box", "claude-boxlite"), "");
    }

    #[test]
    fn format_row_renders_a_missing_origin_as_an_empty_cell_not_a_placeholder() {
        let row = Row {
            name: "claude-boxlite".into(),
            id: "abc12345".into(),
            status: "running".into(),
            image: "claude-boxlite-custom".into(),
            created: "2026-08-18 09:00".into(),
            origin: String::new(),
            marker: " <- here".into(),
        };
        let rendered = format_row(&row);
        assert!(rendered.contains("claude-boxlite"));
        assert!(rendered.ends_with(" <- here"));
        // The empty origin cell is still full-width padding, not "N/A" or
        // similar — nothing but spaces where the path would be.
        assert!(rendered.contains(&" ".repeat(ORIGIN_W)));
    }
}
