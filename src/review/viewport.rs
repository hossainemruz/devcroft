//! Keep the visible code anchored when a refreshed diff changes its row indices.
use super::{LoadedReview, model::FileContent, stream::StreamRow};
use gpui_kit::{ListOffset, px};
use similar::{Algorithm, DiffOp, capture_diff_slices_by_key_deadline};
use std::time::{Duration, Instant};

pub(super) fn restore(old: &LoadedReview, new: &LoadedReview, top: ListOffset) -> ListOffset {
    let Some(row) = old.rows.get(top.item_ix) else {
        // The final item contains comments outside the diff.
        return ListOffset {
            item_ix: new.rows.len(),
            offset_in_item: top.offset_in_item,
        };
    };
    let old_file = row.file();
    let path = &old.diff.files[old_file].path;
    let Some(new_file) = new.diff.files.iter().position(|file| &file.path == path) else {
        // The current file disappeared. Prefer the next surviving file, then
        // the previous one, instead of jumping to the start of the review.
        let neighbor = old.diff.files[old_file + 1..]
            .iter()
            .chain(old.diff.files[..old_file].iter().rev())
            .find_map(|file| new.diff.files.iter().position(|new| new.path == file.path));
        return ListOffset {
            item_ix: neighbor.map_or(0, |ix| new.file_row_start[ix]),
            offset_in_item: px(0.),
        };
    };
    let start = new.file_row_start[new_file];
    let end = new
        .file_row_start
        .get(new_file + 1)
        .copied()
        .unwrap_or(new.rows.len());
    let relative_row = top.item_ix - old.file_row_start[old_file];
    let mut item_ix = (start + relative_row).min(end.saturating_sub(1));
    if matches!(row, StreamRow::Line { .. })
        && old.diff.files[old_file].content != new.diff.files[new_file].content
    {
        let old_lines = lines(old, old_file);
        let new_lines = lines(new, new_file);
        if let Some(old_ix) = old_lines.iter().position(|(ix, _)| *ix == top.item_ix) {
            // Match line content and diff side, ignoring shifted line numbers.
            // The deadline bounds work on the UI thread for large replacements.
            let ops = capture_diff_slices_by_key_deadline(
                Algorithm::Myers,
                &old_lines,
                &new_lines,
                |(_, key)| *key,
                Some(Instant::now() + Duration::from_millis(5)),
            );
            let nearest = ops
                .iter()
                .filter_map(|op| {
                    let DiffOp::Equal {
                        old_index,
                        new_index,
                        len,
                    } = *op
                    else {
                        return None;
                    };
                    let matched = old_ix.clamp(old_index, old_index + len - 1);
                    Some((old_ix.abs_diff(matched), new_index + matched - old_index))
                })
                .min_by_key(|(distance, _)| *distance);
            if let Some((_, new_ix)) = nearest {
                item_ix = new_lines[new_ix].0;
            }
        }
    }
    ListOffset {
        item_ix,
        offset_in_item: top.offset_in_item,
    }
}

fn lines(loaded: &LoadedReview, file: usize) -> Vec<(usize, (u8, &str))> {
    let FileContent::Text { hunks, .. } = &loaded.diff.files[file].content else {
        return Vec::new();
    };
    let start = loaded.file_row_start[file];
    let end = loaded
        .file_row_start
        .get(file + 1)
        .copied()
        .unwrap_or(loaded.rows.len());
    loaded.rows[start..end]
        .iter()
        .enumerate()
        .filter_map(|(ix, row)| {
            let StreamRow::Line {
                file: row_file,
                hunk,
                line,
            } = *row
            else {
                return None;
            };
            if row_file != file {
                return None;
            }
            let line = &hunks[hunk].lines[line];
            Some((start + ix, (line.tag as u8, line.text.as_str())))
        })
        .collect()
}
