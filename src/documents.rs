//! `document` section (MINI_ORK_IDE_SPEC=2): a preview of a file attached to a
//! thread, built by `mini_ork/ide_pages/spec.py:document()` from
//! `mini_ork.documents.extract`. The payload contract is that function's
//! docstring; `path` is absolute in every kind.
//!
//! - `grid` (CSV/XLSX) is drawn by the `table` renderer — [`grid_table`] turns
//!   the document into a `table` section, so a sheet reads like every other
//!   table in the IDE.
//! - `text` (DOCX/PPTX) draws the document's own blocks, selectable.
//! - `pages` (PDF) draws the rasterised page PNGs, read from disk by path — the
//!   page JSON never carries image bytes.

use std::path::PathBuf;

use gpui::{AnyElement, Context, ObjectFit, SharedString, StyledImage as _};
use serde_json::{Value, json};
use ui::prelude::*;

use crate::{
    page::{MiniOrkPage, arr, str_of},
    palette::{self as p, c},
    select::Look,
};

/// A `grid` document as a `table` section, so it shares the table renderer.
pub(crate) fn grid_table(s: &Value) -> Value {
    let columns = arr(s, "columns");
    let rows = arr(s, "rows");
    let total = s.get("total_rows").and_then(Value::as_u64).unwrap_or(rows.len() as u64);
    let mut note = format!("{} of {total} rows", rows.len());
    if let Some(sheet) = s.get("sheet").and_then(Value::as_str) {
        note = format!("sheet {sheet} · {note}");
    }
    let cols: Vec<Value> = columns.iter().map(|_| json!({ "fr": 1.0, "min": 64.0 })).collect();
    let rows: Vec<Value> = rows
        .iter()
        .map(|row| {
            let cells: Vec<Value> = row
                .as_array()
                .map(|cells| cells.iter().map(|cell| json!({ "t": cell_text(cell), "mono": true })).collect())
                .unwrap_or_default();
            json!({ "cells": cells })
        })
        .collect();
    json!({
        "type": "table",
        "title": str_of(s, "title"),
        "note": note,
        "cols": cols,
        "head": columns,
        "rows": rows,
        "actions": open_action(s),
        "full": s.get("full").cloned().unwrap_or(Value::Bool(true)),
    })
}

/// The header button that opens the attachment itself, when the payload names it.
pub(crate) fn open_action(s: &Value) -> Vec<Value> {
    let path = str_of(s, "path");
    if path.is_empty() {
        Vec::new()
    } else {
        vec![json!({ "label": "Open", "kind": "ghost", "do": { "path": path } })]
    }
}

fn cell_text(cell: &Value) -> String {
    match cell {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

impl MiniOrkPage {
    /// The body of a `text` or `pages` document; `grid` never reaches here.
    pub(crate) fn render_document_body(&self, index: usize, s: &Value, cx: &mut Context<Self>) -> AnyElement {
        match str_of(s, "kind").as_str() {
            "text" => self.render_document_text(index, s, cx),
            "pages" => render_document_pages(s),
            other => self.texts.text(
                format!("s{index}-doc-unknown"),
                &format!("This document preview ({other}) needs a newer mini-ork IDE."),
                Look::new(12., c(p::MUTED)),
                cx,
            ),
        }
    }

    fn render_document_text(&self, index: usize, s: &Value, cx: &mut Context<Self>) -> AnyElement {
        let mut blocks: Vec<String> = arr(s, "blocks")
            .iter()
            .filter_map(|block| block.as_str().map(str::to_string))
            .filter(|block| !block.trim().is_empty())
            .collect();
        if blocks.is_empty() {
            blocks.push(str_of(s, "text"));
        }
        let body = v_flex().gap_1p5().children(blocks.iter().enumerate().map(|(i, block)| {
            self.texts.text(format!("s{index}-doc-b{i}"), block, Look::new(13., c(p::BODY)), cx)
        }));
        let key = format!("s{index}-doc-box");
        self.texts
            .nested_scroll(
                key.clone(),
                div()
                    .id(SharedString::from(key))
                    .max_h(rems_from_px(640_f32))
                    .overflow_y_scroll()
                    .pr_1p5()
                    .child(body),
            )
            .into_any_element()
    }
}

fn render_document_pages(s: &Value) -> AnyElement {
    let pages = arr(s, "pages");
    v_flex()
        .gap_3()
        .children(pages.iter().map(|page| {
            let n = page.get("page").and_then(Value::as_u64).unwrap_or(0);
            v_flex()
                .gap_1()
                .child(p::text(format!("page {n}"), 11., p::SUB))
                .child(
                    gpui::img(PathBuf::from(str_of(page, "png")))
                        .w_full()
                        .max_w(rems_from_px(820_f32))
                        .h(rems_from_px(560_f32))
                        .object_fit(ObjectFit::Contain),
                )
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grid_becomes_a_table_with_every_cell_as_text() {
        let doc = json!({
            "type": "document", "kind": "grid", "title": "q3.csv", "path": "/h/a/q3.csv",
            "columns": ["name", "count"], "rows": [["alpha", 3], [null, 4.5]],
            "total_rows": 9, "truncated": true,
        });
        let table = grid_table(&doc);
        assert_eq!(table["type"], "table");
        assert_eq!(table["head"], json!(["name", "count"]));
        assert_eq!(table["rows"][0]["cells"][1]["t"], "3");
        assert_eq!(table["rows"][1]["cells"][0]["t"], "");
        assert_eq!(table["note"], "2 of 9 rows");
        assert_eq!(table["actions"][0]["do"]["path"], "/h/a/q3.csv");
    }

    #[test]
    fn a_sheet_name_rides_the_note_and_no_path_means_no_open_button() {
        let doc = json!({ "kind": "grid", "columns": [], "rows": [], "sheet": "Q3" });
        let table = grid_table(&doc);
        assert_eq!(table["note"], "sheet Q3 · 0 of 0 rows");
        assert_eq!(table["actions"], json!([]));
    }
}
