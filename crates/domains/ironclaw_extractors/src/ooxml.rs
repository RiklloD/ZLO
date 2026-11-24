//! OOXML (docx / pptx / xlsx) extraction: ZIP entries read under per-entry
//! and cumulative decompression caps, then reduced to text.

use std::io::Read;

/// Maximum decompressed size for a single ZIP entry (50 MB).
pub(crate) const MAX_DECOMPRESSED_ENTRY: u64 = 50 * 1024 * 1024;

/// Maximum total decompressed size across all ZIP entries (100 MB).
pub(crate) const MAX_DECOMPRESSED_TOTAL: u64 = 100 * 1024 * 1024;

/// Typed errors for ZIP decompression safety checks.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ZipEntryError {
    #[error("entry '{name}' decompressed size {size} exceeds per-entry limit {max}")]
    EntryTooLarge { name: String, size: u64, max: u64 },

    #[error("total decompressed size {current} exceeds limit {limit}")]
    TotalSizeLimitExceeded { limit: u64, current: u64 },

    #[error("failed to read zip entry '{name}': {source}")]
    EntryReadFailed {
        name: String,
        source: std::io::Error,
    },
    #[error("zip entry name could not be decoded: {source}")]
    EntryNameInvalid { source: zip::result::ZipError },
}

/// Read a zip entry into a string with configurable decompressed size limits.
pub(crate) fn bounded_read_zip_entry_with_limits<R: Read + ?Sized>(
    file: &mut zip::read::ZipFile<'_, R>,
    total_decompressed: &mut u64,
    max_entry: u64,
    max_total: u64,
) -> Result<String, ZipEntryError> {
    let entry_size = file.size();
    let entry_name = file
        .name()
        .map_err(|source| ZipEntryError::EntryNameInvalid { source })?
        .into_owned();

    // Fast pre-check using declared header size (untrusted, but cheap reject)
    // against per-entry limit.
    if entry_size > max_entry {
        return Err(ZipEntryError::EntryTooLarge {
            name: entry_name,
            size: entry_size,
            max: max_entry,
        });
    }

    // Pre-check: reject early if the declared size would blow the cumulative
    // budget. The header value is untrusted, but it lets us reject obviously
    // oversized archives without decompressing.
    if *total_decompressed + entry_size > max_total {
        return Err(ZipEntryError::TotalSizeLimitExceeded {
            limit: max_total,
            current: *total_decompressed + entry_size,
        });
    }

    let mut bounded = file.take(max_entry);
    let mut xml = String::new();
    bounded
        .read_to_string(&mut xml)
        .map_err(|e| ZipEntryError::EntryReadFailed {
            name: entry_name.clone(),
            source: e,
        })?;

    let actual_size = xml.len() as u64;

    // Fail closed: if we read exactly the cap, the entry was truncated and
    // the real decompressed size exceeds the per-entry limit.
    if actual_size >= max_entry {
        return Err(ZipEntryError::EntryTooLarge {
            name: entry_name,
            size: actual_size,
            max: max_entry,
        });
    }

    // Track cumulative budget using actual bytes, not header metadata.
    *total_decompressed += actual_size;
    if *total_decompressed > max_total {
        return Err(ZipEntryError::TotalSizeLimitExceeded {
            limit: max_total,
            current: *total_decompressed,
        });
    }

    Ok(xml)
}

/// Read a zip entry into a string with default decompressed size limits.
///
/// Uses the declared header size as a fast pre-check for both per-entry and
/// cumulative budgets, then tracks **actual bytes read** for the cumulative
/// budget (ZIP headers can lie about sizes). The `take()` reader caps any
/// single entry at `MAX_DECOMPRESSED_ENTRY`. If the reader hits that cap
/// exactly we fail closed — the entry was truncated, meaning the real size
/// exceeds the limit.
pub(crate) fn bounded_read_zip_entry<R: Read + ?Sized>(
    file: &mut zip::read::ZipFile<'_, R>,
    total_decompressed: &mut u64,
) -> Result<String, ZipEntryError> {
    bounded_read_zip_entry_with_limits(
        file,
        total_decompressed,
        MAX_DECOMPRESSED_ENTRY,
        MAX_DECOMPRESSED_TOTAL,
    )
}

pub(crate) fn extract_docx(data: &[u8]) -> Result<String, String> {
    extract_office_xml(data, "word/document.xml")
}

pub(crate) fn extract_pptx(data: &[u8]) -> Result<String, String> {
    let cursor = std::io::Cursor::new(data);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("invalid PPTX archive: {e}"))?;

    // Collect slide filenames (ppt/slides/slide1.xml, slide2.xml, ...)
    // Names the archive cannot decode are skipped.
    let mut slide_names: Vec<String> = archive
        .file_names()
        .filter_map(Result::ok)
        .filter(|name| name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
        .map(std::borrow::Cow::into_owned)
        .collect();
    slide_names.sort();

    let mut all_text = Vec::new();
    let mut total_decompressed: u64 = 0;
    let mut rejected: Option<String> = None;
    for name in &slide_names {
        let Ok(mut file) = archive.by_name(name) else {
            rejected.get_or_insert_with(|| format!("could not open PPTX slide {name}"));
            continue;
        };
        let xml = match bounded_read_zip_entry(&mut file, &mut total_decompressed) {
            Ok(xml) => xml,
            Err(error) => {
                rejected.get_or_insert_with(|| error.to_string());
                continue;
            }
        };
        let text = strip_xml_tags(&xml);
        if !text.is_empty() {
            all_text.push(text);
        }
    }

    // …but "no text" is only the truth when nothing was *refused*. Entries that
    // trip the decompression bounds are skipped silently, and before #7104 the
    // empty-result error was the only thing that surfaced them. Keep that
    // signal: if the guard rejected an entry and nothing else yielded text, the
    // file failed — it is not text-free.
    if all_text.is_empty()
        && let Some(reason) = rejected
    {
        return Err(reason);
    }

    // Ran fine, found nothing. `Ok(String::new())` so `extract_document`'s
    // trim-and-classify produces `Empty`, which renders "[No extractable text
    // found …]" — the truth. Returning `Err` here made a well-formed, image-only
    // file read as "[Could not extract text …]", inviting a retry that cannot
    // help (#7104).
    Ok(all_text.join("\n\n---\n\n"))
}

pub(crate) fn extract_xlsx(data: &[u8]) -> Result<String, String> {
    let cursor = std::io::Cursor::new(data);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("invalid XLSX archive: {e}"))?;

    let mut total_decompressed: u64 = 0;

    // Read shared strings (xl/sharedStrings.xml)
    let shared_strings = if let Ok(mut file) = archive.by_name("xl/sharedStrings.xml") {
        let xml = bounded_read_zip_entry(&mut file, &mut total_decompressed)
            .map_err(|e| format!("failed to read shared strings: {e}"))?;
        parse_xlsx_shared_strings(&xml)
    } else {
        Vec::new()
    };

    // Read sheet data
    let mut sheet_names: Vec<String> = archive
        .file_names()
        .filter_map(Result::ok)
        .filter(|name| name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml"))
        .map(std::borrow::Cow::into_owned)
        .collect();
    sheet_names.sort();

    let mut all_text = Vec::new();
    let mut rejected: Option<String> = None;
    for name in &sheet_names {
        let Ok(mut file) = archive.by_name(name) else {
            rejected.get_or_insert_with(|| format!("could not open XLSX sheet {name}"));
            continue;
        };
        let xml = match bounded_read_zip_entry(&mut file, &mut total_decompressed) {
            Ok(xml) => xml,
            Err(error) => {
                rejected.get_or_insert_with(|| error.to_string());
                continue;
            }
        };
        let text = parse_xlsx_sheet(&xml, &shared_strings);
        if !text.is_empty() {
            all_text.push(text);
        }
    }

    if all_text.is_empty() && !shared_strings.is_empty() {
        // Fallback: just return shared strings
        return Ok(shared_strings.join("\n"));
    }

    // …but "no text" is only the truth when nothing was *refused*. Entries that
    // trip the decompression bounds are skipped silently, and before #7104 the
    // empty-result error was the only thing that surfaced them. Keep that
    // signal: if the guard rejected an entry and nothing else yielded text, the
    // file failed — it is not text-free.
    if all_text.is_empty()
        && let Some(reason) = rejected
    {
        return Err(reason);
    }

    // Ran fine, found nothing. `Ok(String::new())` so `extract_document`'s
    // trim-and-classify produces `Empty`, which renders "[No extractable text
    // found …]" — the truth. Returning `Err` here made a well-formed, image-only
    // file read as "[Could not extract text …]", inviting a retry that cannot
    // help (#7104).
    Ok(all_text.join("\n\n"))
}

pub(crate) fn extract_office_xml(data: &[u8], content_path: &str) -> Result<String, String> {
    let cursor = std::io::Cursor::new(data);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("invalid Office XML archive: {e}"))?;

    let mut file = archive
        .by_name(content_path)
        .map_err(|e| format!("content file not found in archive: {e}"))?;

    let mut total_decompressed: u64 = 0;
    let xml = bounded_read_zip_entry(&mut file, &mut total_decompressed)
        .map_err(|e| format!("failed to read content: {e}"))?;

    let text = strip_xml_tags(&xml);
    // Ran fine, found nothing. `Ok(String::new())` so `extract_document`'s
    // trim-and-classify produces `Empty`, which renders "[No extractable text
    // found …]" — the truth. Returning `Err` here made a well-formed, image-only
    // file read as "[Could not extract text …]", inviting a retry that cannot
    // help (#7104).

    Ok(text)
}

/// Strip XML tags and return just the text content.
pub(crate) fn strip_xml_tags(xml: &str) -> String {
    let mut result = String::with_capacity(xml.len() / 2);
    let mut in_tag = false;
    let mut last_was_space = true;

    for ch in xml.chars() {
        match ch {
            '<' => {
                in_tag = true;
            }
            '>' => {
                in_tag = false;
                // Add space between tag-delimited text runs
                if !last_was_space && !result.is_empty() {
                    result.push(' ');
                    last_was_space = true;
                }
            }
            _ if !in_tag => {
                if ch.is_whitespace() {
                    if !last_was_space {
                        result.push(' ');
                        last_was_space = true;
                    }
                } else {
                    result.push(ch);
                    last_was_space = false;
                }
            }
            _ => {}
        }
    }

    // Decode common XML entities
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .trim()
        .to_string()
}

/// Parse XLSX shared strings XML into a Vec of strings.
pub(crate) fn parse_xlsx_shared_strings(xml: &str) -> Vec<String> {
    // Shared strings are in <si><t>text</t></si> elements
    let mut strings = Vec::new();
    let mut in_t = false;
    let mut current = String::new();
    let mut in_tag = false;
    let mut tag_name = String::new();

    for ch in xml.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag_name.clear();
            }
            '>' => {
                in_tag = false;
                let tag = tag_name.trim().to_string();
                if tag == "t" || tag.starts_with("t ") {
                    in_t = true;
                    current.clear();
                } else if tag == "/t" {
                    in_t = false;
                    strings.push(std::mem::take(&mut current));
                } else if tag == "/si" {
                    in_t = false;
                }
            }
            _ if in_tag => {
                tag_name.push(ch);
            }
            _ if in_t => {
                current.push(ch);
            }
            _ => {}
        }
    }

    strings
}

/// Parse XLSX sheet XML into tab-separated rows.
pub(crate) fn parse_xlsx_sheet(xml: &str, shared_strings: &[String]) -> String {
    // Simple extraction: find <v> values in <c> cells, resolve shared string refs
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut in_v = false;
    let mut in_row = false;
    let mut current_val = String::new();
    let mut cell_type = String::new();
    let mut in_tag = false;
    let mut tag_buf = String::new();

    for ch in xml.chars() {
        match ch {
            '<' => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' => {
                in_tag = false;
                let tag = tag_buf.trim().to_string();
                if tag == "row" || tag.starts_with("row ") {
                    in_row = true;
                    current_row.clear();
                } else if tag == "/row" {
                    in_row = false;
                    if !current_row.is_empty() {
                        rows.push(std::mem::take(&mut current_row));
                    }
                } else if in_row && (tag.starts_with("c ") || tag == "c") {
                    // Extract type attribute: t="s" means shared string
                    cell_type.clear();
                    if let Some(t_pos) = tag.find("t=\"") {
                        let rest = &tag[t_pos + 3..];
                        if let Some(end) = rest.find('"') {
                            // `end` comes from `find` on an ASCII quote: a char boundary.
                            cell_type = rest[..end].to_string(); // safety: ASCII boundary
                        }
                    }
                } else if tag == "v" || tag.starts_with("v ") {
                    in_v = true;
                    current_val.clear();
                } else if tag == "/v" {
                    in_v = false;
                    let val = if cell_type == "s" {
                        // Shared string reference
                        current_val
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|idx| shared_strings.get(idx))
                            .cloned()
                            .unwrap_or_default()
                    } else {
                        current_val.clone()
                    };
                    current_row.push(val);
                } else if tag == "/c" {
                    cell_type.clear();
                }
            }
            _ if in_tag => {
                tag_buf.push(ch);
            }
            _ if in_v => {
                current_val.push(ch);
            }
            _ => {}
        }
    }

    rows.iter()
        .map(|row| row.join("\t"))
        .collect::<Vec<_>>()
        .join("\n")
}
