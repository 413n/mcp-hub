//! What the hub may do to a tool's result before passing it on, to save the agent context:
//! drop empty fields (`compactResults`), and keep a very large result in a file instead
//! of passing it on whole (`maxResult`).
//!
//! A result can hold the same data twice, as text and as `structuredContent`. Which one an
//! agent shows its model differs (Claude Code: the structured one if there is one; Codex:
//! both), so both are treated alike.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

use crate::stats::private_dir;

/// How much of a result that was not passed on whole the agent still gets to see.
const PREVIEW_CHARS: usize = 2000;
/// How long the file with the whole result is kept.
const KEPT_FOR: Duration = Duration::from_secs(3600);

/// A JSON object or array in `text`. Anything else, a bare number included, is left as text.
fn json_in(text: &str) -> Option<Value> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with(['{', '[']) {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

/// Remove the members of objects that say nothing: null, "", [] and {}, also those that
/// are only left empty by this. Array elements stay, as their position can mean something.
fn prune(value: &mut Value) {
    match value {
        Value::Object(members) => {
            members.values_mut().for_each(prune);
            members.retain(|_, member| match member {
                Value::Null => false,
                Value::String(text) => !text.is_empty(),
                Value::Array(items) => !items.is_empty(),
                Value::Object(members) => !members.is_empty(),
                _ => true,
            });
        }
        Value::Array(items) => items.iter_mut().for_each(prune),
        _ => {}
    }
}

/// The size of a result as an agent's model gets it: the larger of its text and its
/// structured content, in characters.
pub fn size(result: &CallToolResult) -> usize {
    let texts = result.content.iter().filter_map(ContentBlock::as_text);
    let text: usize = texts.map(|text| text.text.chars().count()).sum();
    let structured = result.structured_content.as_ref();
    text.max(structured.map_or(0, |value| value.to_string().len()))
}

/// Drop the empty fields from the JSON in `result`, text and structured alike.
pub fn compact(result: &mut CallToolResult) {
    for block in &mut result.content {
        if let ContentBlock::Text(text) = block
            && let Some(mut value) = json_in(&text.text)
        {
            prune(&mut value);
            text.text = value.to_string();
        }
    }
    if let Some(value) = &mut result.structured_content {
        prune(value);
    }
}

/// Delete the results in `dir` that have been kept long enough.
pub fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let modified = entry.metadata().and_then(|m| m.modified());
        let age = modified
            .ok()
            .and_then(|at| SystemTime::now().duration_since(at).ok());
        if age.is_some_and(|age| age > KEPT_FOR) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Write the whole of `result` to a new file in `dir`: as indented JSON if that is what
/// it is, so that it can be read by lines, else as the text it is.
fn keep(result: &CallToolResult, dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    private_dir(dir)?;
    sweep(dir);
    let texts = result.content.iter().filter_map(ContentBlock::as_text);
    let text: Vec<&str> = texts.map(|text| text.text.as_str()).collect();
    let text = text.join("\n");
    let json = result.structured_content.clone().or_else(|| json_in(&text));
    let (extension, contents) = match json.map(|json| serde_json::to_string_pretty(&json)) {
        Some(Ok(pretty)) => ("json", pretty),
        _ => ("txt", text),
    };
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
    let path = dir.join(format!(
        "{name}-{}-{}.{extension}",
        now.map_or(0, |d| d.as_secs()),
        COUNT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(&path)?, contents.as_bytes())?;
    Ok(path)
}

/// If `result` is larger than `max` characters, keep it in a file in `dir` and leave in
/// its place where that file is and how the result starts. Returns the size it had.
///
/// `name` names the tool in the file's name. A result that cannot be written to a file is
/// passed on whole: too much is better than nothing.
pub fn cap(result: &mut CallToolResult, max: usize, dir: &Path, name: &str) -> Option<usize> {
    let size = size(result);
    if size <= max {
        return None;
    }
    let path = match keep(result, dir, name) {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!("cannot keep a large result in {}: {e}", dir.display());
            return None;
        }
    };
    let texts = result.content.iter().filter_map(ContentBlock::as_text);
    let mut start: String = texts
        .flat_map(|text| text.text.chars())
        .take(PREVIEW_CHARS)
        .collect();
    if start.is_empty()
        && let Some(value) = &result.structured_content
    {
        start = value.to_string().chars().take(PREVIEW_CHARS).collect();
    }
    let note = format!(
        "[mcp-hub: this result is {size} characters, more than the {max} it passes on at once \
         (maxResult). All of it is in the file {}, kept for an hour: read what you need from \
         there, for example with jq or grep, or a range of its lines. It starts:]\n{start}",
        path.display()
    );
    // Images and the like stay; the text and its structured twin are what was too large.
    result.content.retain(|block| block.as_text().is_none());
    result.content.insert(0, ContentBlock::text(note));
    result.structured_content = None;
    Some(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_of(result: &CallToolResult) -> &str {
        &result.content[0].as_text().unwrap().text
    }

    #[test]
    fn drops_empty_fields() {
        let data = json!({
            "id": 1, "name": "", "tags": [], "meta": {"a": null, "b": {}}, "zero": 0, "no": false,
            "items": [null, {"x": null, "y": 2}, []],
        });
        let mut result = CallToolResult::success(vec![
            ContentBlock::text(data.to_string()),
            ContentBlock::text("not JSON: {"),
            ContentBlock::text("12"),
        ]);
        result.structured_content = Some(data);
        compact(&mut result);
        let kept = json!({"id": 1, "zero": 0, "no": false, "items": [null, {"y": 2}, []]});
        assert_eq!(text_of(&result), kept.to_string());
        assert_eq!(result.structured_content, Some(kept));
        assert_eq!(result.content[1].as_text().unwrap().text, "not JSON: {");
        assert_eq!(result.content[2].as_text().unwrap().text, "12");
    }

    #[test]
    fn keeps_a_large_result_in_a_file() {
        let dir = std::env::temp_dir().join(format!("mcp-hub-results-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rows: Vec<_> = (0..500).map(|n| json!({"n": n, "name": "row"})).collect();
        let data = json!({"rows": rows});
        let whole = data.to_string();

        let mut small = CallToolResult::success(vec![ContentBlock::text("fine")]);
        assert_eq!(cap(&mut small, 1000, &dir, "s-t"), None);
        assert_eq!(text_of(&small), "fine");

        // Text and structured content alike; an image is not text and stays.
        for structured in [false, true] {
            let image = ContentBlock::image("aGk=", "image/png");
            let mut result = CallToolResult::success(vec![ContentBlock::text(&whole), image]);
            result.structured_content = structured.then(|| data.clone());
            assert_eq!(cap(&mut result, 1000, &dir, "s-t"), Some(whole.len()));
            assert_eq!(
                (result.content.len(), &result.structured_content),
                (2, &None)
            );
            let note = text_of(&result);
            assert!(
                note.len() < 3000 && note.ends_with(&whole[..PREVIEW_CHARS]),
                "{note}"
            );
            let path = note
                .split("in the file ")
                .nth(1)
                .unwrap()
                .split(", kept")
                .next()
                .unwrap();
            assert!(
                path.starts_with(dir.to_str().unwrap()) && path.ends_with(".json"),
                "{path}"
            );
            let kept: Value =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert_eq!(kept, data);
            assert!(std::fs::read_to_string(path).unwrap().lines().count() > 500);
        }

        let mut prose = CallToolResult::success(vec![ContentBlock::text("word ".repeat(1000))]);
        assert_eq!(cap(&mut prose, 1000, &dir, "s-t"), Some(5000));
        assert!(text_of(&prose).contains(".txt, kept for an hour"));

        // Nowhere to write: the result is passed on as it is.
        let mut result = CallToolResult::success(vec![ContentBlock::text(&whole)]);
        assert_eq!(
            cap(&mut result, 1000, Path::new("/proc/nowhere"), "s-t"),
            None
        );
        assert_eq!(text_of(&result), whole);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
