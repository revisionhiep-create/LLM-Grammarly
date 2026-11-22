use anyhow::{bail, Context, Result};
use eframe::egui;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::backup::{atomic_save_new_file, atomic_save_with_backup};
use crate::docx::{read_docx, validate_docx, write_patched_docx, DocxDocument};
use crate::llm::{LlmHandle, PromptMode};

const TARGET_BATCH_WORDS: usize = 260;
const MAX_BATCH_WORDS: usize = 350;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocFormat {
    Txt,
    Markdown,
    Docx,
}

impl DocFormat {
    pub fn from_path(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "txt" => Ok(Self::Txt),
            "md" | "markdown" => Ok(Self::Markdown),
            "docx" => Ok(Self::Docx),
            _ => bail!("Unsupported file type. Supported formats: .txt, .md, .docx"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Txt => "TXT",
            Self::Markdown => "MD",
            Self::Docx => "DOCX",
        }
    }

    pub fn default_extension(self) -> &'static str {
        match self {
            Self::Txt => "txt",
            Self::Markdown => "md",
            Self::Docx => "docx",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewScope {
    #[default]
    OneBatch,
    WholeFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewDecision {
    #[default]
    Pending,
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffOp {
    Equal(String),
    Delete(String),
    Insert(String),
}

#[derive(Debug, Clone)]
pub enum TextPiece {
    Passthrough(String),
    Editable(usize),
}

#[derive(Debug, Clone)]
pub struct ReviewItem {
    pub id: usize,
    pub docx_key: Option<(usize, usize)>,
    pub original: String,
    pub suggested: Option<String>,
    pub diff_ops: Vec<DiffOp>,
    pub decision: ReviewDecision,
    pub length_delta_pct: Option<f32>,
    pub error: Option<String>,
}

impl ReviewItem {
    pub fn has_change(&self) -> bool {
        self.suggested
            .as_deref()
            .is_some_and(|s| s != self.original)
    }
}

#[derive(Debug, Clone)]
pub struct ReviewBatch {
    pub item_indices: Vec<usize>,
    pub word_count: usize,
}

#[derive(Debug, Clone)]
pub struct LoadedDocument {
    pub path: PathBuf,
    pub format: DocFormat,
    pub pieces: Vec<TextPiece>,
    pub docx: Option<DocxDocument>,
    pub items: Vec<ReviewItem>,
    pub batches: Vec<ReviewBatch>,
    pub had_crlf: bool,
}

impl LoadedDocument {
    pub fn load(path: PathBuf) -> Result<Self> {
        let format = DocFormat::from_path(&path)?;
        match format {
            DocFormat::Txt => {
                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read UTF-8 text file {}", path.display()))?;
                let had_crlf = raw.contains("\r\n");
                let normalized = raw.replace("\r\n", "\n");
                let (pieces, items) = parse_txt_pieces(&normalized);
                let batches = build_batches(&items);
                Ok(Self {
                    path,
                    format,
                    pieces,
                    docx: None,
                    items,
                    batches,
                    had_crlf,
                })
            }
            DocFormat::Markdown => {
                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read UTF-8 Markdown file {}", path.display()))?;
                let had_crlf = raw.contains("\r\n");
                let normalized = raw.replace("\r\n", "\n");
                let (pieces, items) = parse_markdown_pieces(&normalized);
                let batches = build_batches(&items);
                Ok(Self {
                    path,
                    format,
                    pieces,
                    docx: None,
                    items,
                    batches,
                    had_crlf,
                })
            }
            DocFormat::Docx => {
                let docx = read_docx(&path)?;
                let items: Vec<ReviewItem> = docx
                    .segments
                    .iter()
                    .enumerate()
                    .map(|(id, seg)| ReviewItem {
                        id,
                        docx_key: Some((seg.para_index, seg.seg_index)),
                        original: seg.text.clone(),
                        suggested: None,
                        diff_ops: Vec::new(),
                        decision: ReviewDecision::Pending,
                        length_delta_pct: None,
                        error: None,
                    })
                    .collect();
                let batches = build_batches(&items);
                Ok(Self {
                    path,
                    format,
                    pieces: Vec::new(),
                    docx: Some(docx),
                    items,
                    batches,
                    had_crlf: false,
                })
            }
        }
    }

    pub fn render_text_output(&self) -> String {
        let mut out = String::new();
        for piece in &self.pieces {
            match piece {
                TextPiece::Passthrough(s) => out.push_str(s),
                TextPiece::Editable(idx) => {
                    let item = &self.items[*idx];
                    if item.decision == ReviewDecision::Accepted {
                        if let Some(sug) = item.suggested.as_deref() {
                            out.push_str(sug);
                            continue;
                        }
                    }
                    out.push_str(&item.original);
                }
            }
        }
        if self.had_crlf {
            out.replace('\n', "\r\n")
        } else {
            out
        }
    }

    pub fn accepted_docx_replacements(&self) -> HashMap<(usize, usize), String> {
        let mut map = HashMap::new();
        for item in &self.items {
            if item.decision == ReviewDecision::Accepted {
                if let (Some(key), Some(sug)) = (item.docx_key, item.suggested.as_ref()) {
                    if sug != &item.original {
                        map.insert(key, sug.clone());
                    }
                }
            }
        }
        map
    }

    pub fn stats(&self) -> (usize, usize, usize, usize) {
        let mut reviewed = 0usize;
        let mut accepted = 0usize;
        let mut pending_reviewed = 0usize;
        let mut flagged_pending = 0usize;
        for item in &self.items {
            if item.suggested.is_some() {
                reviewed += 1;
                if item.has_change() {
                    match item.decision {
                        ReviewDecision::Accepted => accepted += 1,
                        ReviewDecision::Pending => {
                            pending_reviewed += 1;
                            if item.length_delta_pct.is_some() {
                                flagged_pending += 1;
                            }
                        }
                        ReviewDecision::Rejected => {}
                    }
                }
            }
        }
        (reviewed, accepted, pending_reviewed, flagged_pending)
    }

    pub fn accept_all_in_batch(&mut self, batch_idx: usize) {
        let Some(batch) = self.batches.get(batch_idx) else {
            return;
        };
        for &idx in &batch.item_indices {
            if self.items[idx].has_change() {
                self.items[idx].decision = ReviewDecision::Accepted;
            }
        }
    }
}

fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

fn build_batches(items: &[ReviewItem]) -> Vec<ReviewBatch> {
    let mut batches: Vec<ReviewBatch> = Vec::new();
    let mut cur_indices: Vec<usize> = Vec::new();
    let mut cur_words = 0usize;

    for (idx, item) in items.iter().enumerate() {
        let wc = word_count(&item.original).max(1);
        if !cur_indices.is_empty()
            && (cur_words + wc > MAX_BATCH_WORDS || cur_words >= TARGET_BATCH_WORDS)
        {
            batches.push(ReviewBatch {
                item_indices: std::mem::take(&mut cur_indices),
                word_count: cur_words,
            });
            cur_words = 0;
        }
        cur_indices.push(idx);
        cur_words += wc;
    }

    if !cur_indices.is_empty() {
        batches.push(ReviewBatch {
            item_indices: cur_indices,
            word_count: cur_words,
        });
    }

    batches
}

fn split_lines_keep_newline(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (idx, b) in s.bytes().enumerate() {
        if b == b'\n' {
            lines.push(&s[start..=idx]);
            start = idx + 1;
        }
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

fn push_editable_paragraph(
    para_buf: &mut String,
    pieces: &mut Vec<TextPiece>,
    items: &mut Vec<ReviewItem>,
) {
    if para_buf.is_empty() {
        return;
    }
    let raw = std::mem::take(para_buf);
    let trailing_nl = if raw.ends_with('\n') { "\n" } else { "" };
    let body = &raw[..raw.len() - trailing_nl.len()];

    if body.trim().is_empty() {
        pieces.push(TextPiece::Passthrough(raw));
        return;
    }

    let id = items.len();
    items.push(ReviewItem {
        id,
        docx_key: None,
        original: body.to_string(),
        suggested: None,
        diff_ops: Vec::new(),
        decision: ReviewDecision::Pending,
        length_delta_pct: None,
        error: None,
    });
    pieces.push(TextPiece::Editable(id));
    if !trailing_nl.is_empty() {
        pieces.push(TextPiece::Passthrough(trailing_nl.to_string()));
    }
}

pub fn parse_txt_pieces(content_lf: &str) -> (Vec<TextPiece>, Vec<ReviewItem>) {
    let mut pieces = Vec::new();
    let mut items = Vec::new();
    let mut para_buf = String::new();

    for line in split_lines_keep_newline(content_lf) {
        if line.trim().is_empty() {
            push_editable_paragraph(&mut para_buf, &mut pieces, &mut items);
            pieces.push(TextPiece::Passthrough(line.to_string()));
        } else {
            para_buf.push_str(line);
        }
    }
    push_editable_paragraph(&mut para_buf, &mut pieces, &mut items);
    (pieces, items)
}

fn detect_fence_open(line: &str) -> Option<(char, usize)> {
    let stripped = line.trim_start_matches(' ');
    let leading_spaces = line.len() - stripped.len();
    if leading_spaces > 3 {
        return None;
    }
    let first = stripped.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = stripped.chars().take_while(|&c| c == first).count();
    if count >= 3 {
        Some((first, count))
    } else {
        None
    }
}

fn is_fence_close(line: &str, ch: char, min_len: usize) -> bool {
    let stripped = line.trim_start_matches(' ');
    let leading_spaces = line.len() - stripped.len();
    if leading_spaces > 3 {
        return false;
    }
    let count = stripped.chars().take_while(|&c| c == ch).count();
    count >= min_len && stripped[count..].trim().is_empty()
}

fn is_markdown_thematic_break(trimmed: &str) -> bool {
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() < 3 {
        return false;
    }
    let first = compact.chars().next().unwrap_or(' ');
    matches!(first, '-' | '*' | '_') && compact.chars().all(|c| c == first)
}

fn is_markdown_list_item(trimmed: &str) -> bool {
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return true;
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && digits < trimmed.len() {
        let rest = &trimmed[digits..];
        return rest.starts_with(". ") || rest.starts_with(") ");
    }
    false
}

pub fn parse_markdown_pieces(content_lf: &str) -> (Vec<TextPiece>, Vec<ReviewItem>) {
    let lines = split_lines_keep_newline(content_lf);
    let mut pieces = Vec::new();
    let mut items = Vec::new();
    let mut para_buf = String::new();
    let mut i = 0usize;

    // Preserve YAML frontmatter verbatim if present at start of file.
    if lines.first().is_some_and(|l| l.trim() == "---") {
        let mut end_idx = None;
        for (j, line) in lines.iter().enumerate().skip(1) {
            let t = line.trim();
            if t == "---" || t == "..." {
                end_idx = Some(j);
                break;
            }
        }
        if let Some(end) = end_idx {
            let mut fm = String::new();
            for line in &lines[..=end] {
                fm.push_str(line);
            }
            pieces.push(TextPiece::Passthrough(fm));
            i = end + 1;
        }
    }

    let mut active_fence: Option<(char, usize)> = None;
    let mut fence_buf = String::new();

    while i < lines.len() {
        let line = lines[i];
        i += 1;

        if let Some((fch, flen)) = active_fence {
            fence_buf.push_str(line);
            if is_fence_close(line, fch, flen) {
                pieces.push(TextPiece::Passthrough(std::mem::take(&mut fence_buf)));
                active_fence = None;
            }
            continue;
        }

        if let Some((fch, flen)) = detect_fence_open(line) {
            push_editable_paragraph(&mut para_buf, &mut pieces, &mut items);
            active_fence = Some((fch, flen));
            fence_buf.push_str(line);
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty()
            || is_markdown_thematic_break(trimmed)
            || (trimmed.starts_with("![") && trimmed.ends_with(')'))
            || (trimmed.starts_with("<!--") && trimmed.ends_with("-->"))
        {
            push_editable_paragraph(&mut para_buf, &mut pieces, &mut items);
            pieces.push(TextPiece::Passthrough(line.to_string()));
        } else if trimmed.starts_with('#') {
            push_editable_paragraph(&mut para_buf, &mut pieces, &mut items);
            para_buf.push_str(line);
            push_editable_paragraph(&mut para_buf, &mut pieces, &mut items);
        } else if is_markdown_list_item(trimmed) {
            push_editable_paragraph(&mut para_buf, &mut pieces, &mut items);
            para_buf.push_str(line);
        } else {
            para_buf.push_str(line);
        }
    }

    if !fence_buf.is_empty() {
        pieces.push(TextPiece::Passthrough(fence_buf));
    }
    push_editable_paragraph(&mut para_buf, &mut pieces, &mut items);
    (pieces, items)
}

fn tokenize_diff(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut iter = s.char_indices().peekable();
    while let Some((start, ch)) = iter.next() {
        if ch.is_whitespace() {
            let mut end = start + ch.len_utf8();
            while let Some(&(next_i, next_c)) = iter.peek() {
                if next_c.is_whitespace() {
                    end = next_i + next_c.len_utf8();
                    iter.next();
                } else {
                    break;
                }
            }
            out.push(&s[start..end]);
        } else if ch.is_alphanumeric() || ch == '\'' || ch == '-' {
            let mut end = start + ch.len_utf8();
            while let Some(&(next_i, next_c)) = iter.peek() {
                if next_c.is_alphanumeric() || next_c == '\'' || next_c == '-' {
                    end = next_i + next_c.len_utf8();
                    iter.next();
                } else {
                    break;
                }
            }
            out.push(&s[start..end]);
        } else {
            out.push(&s[start..start + ch.len_utf8()]);
        }
    }
    out
}

fn push_diff_op(ops: &mut Vec<DiffOp>, next: DiffOp) {
    match (ops.last_mut(), next) {
        (Some(DiffOp::Equal(prev)), DiffOp::Equal(s)) => prev.push_str(&s),
        (Some(DiffOp::Delete(prev)), DiffOp::Delete(s)) => prev.push_str(&s),
        (Some(DiffOp::Insert(prev)), DiffOp::Insert(s)) => prev.push_str(&s),
        (_, op) => ops.push(op),
    }
}

pub fn compute_word_diff(old: &str, new: &str) -> Vec<DiffOp> {
    if old == new {
        return if old.is_empty() {
            Vec::new()
        } else {
            vec![DiffOp::Equal(old.to_string())]
        };
    }

    let a = tokenize_diff(old);
    let b = tokenize_diff(new);

    let mut prefix = 0usize;
    while prefix < a.len() && prefix < b.len() && a[prefix] == b[prefix] {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < (a.len() - prefix)
        && suffix < (b.len() - prefix)
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let mid_a = &a[prefix..a.len() - suffix];
    let mid_b = &b[prefix..b.len() - suffix];

    let mut ops = Vec::new();
    for &tok in &a[..prefix] {
        push_diff_op(&mut ops, DiffOp::Equal(tok.to_string()));
    }

    let m = mid_a.len();
    let n = mid_b.len();
    if m == 0 {
        for &tok in mid_b {
            push_diff_op(&mut ops, DiffOp::Insert(tok.to_string()));
        }
    } else if n == 0 {
        for &tok in mid_a {
            push_diff_op(&mut ops, DiffOp::Delete(tok.to_string()));
        }
    } else if m.saturating_mul(n) > 250_000 {
        for &tok in mid_a {
            push_diff_op(&mut ops, DiffOp::Delete(tok.to_string()));
        }
        for &tok in mid_b {
            push_diff_op(&mut ops, DiffOp::Insert(tok.to_string()));
        }
    } else {
        let cols = n + 1;
        let mut dp = vec![0u32; (m + 1) * cols];
        for i in (0..m).rev() {
            for j in (0..n).rev() {
                dp[i * cols + j] = if mid_a[i] == mid_b[j] {
                    dp[(i + 1) * cols + (j + 1)] + 1
                } else {
                    dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)])
                };
            }
        }
        let mut i = 0usize;
        let mut j = 0usize;
        while i < m && j < n {
            if mid_a[i] == mid_b[j] {
                push_diff_op(&mut ops, DiffOp::Equal(mid_a[i].to_string()));
                i += 1;
                j += 1;
            } else if dp[(i + 1) * cols + j] >= dp[i * cols + (j + 1)] {
                push_diff_op(&mut ops, DiffOp::Delete(mid_a[i].to_string()));
                i += 1;
            } else {
                push_diff_op(&mut ops, DiffOp::Insert(mid_b[j].to_string()));
                j += 1;
            }
        }
        while i < m {
            push_diff_op(&mut ops, DiffOp::Delete(mid_a[i].to_string()));
            i += 1;
        }
        while j < n {
            push_diff_op(&mut ops, DiffOp::Insert(mid_b[j].to_string()));
            j += 1;
        }
    }

    for &tok in &a[a.len() - suffix..] {
        push_diff_op(&mut ops, DiffOp::Equal(tok.to_string()));
    }

    ops
}

pub fn length_deviation_pct(original: &str, suggested: &str) -> Option<f32> {
    let orig_len = original.chars().count();
    if orig_len < 12 {
        return None;
    }
    let new_len = suggested.chars().count();
    let pct = ((new_len as f32 - orig_len as f32) / (orig_len as f32)) * 100.0;
    if pct.abs() > 25.0 {
        Some(pct)
    } else {
        None
    }
}

fn write_text_file(tmp_path: &Path, content: &str) -> Result<()> {
    let mut f = File::create(tmp_path)
        .with_context(|| format!("Failed to create temporary file {}", tmp_path.display()))?;
    f.write_all(content.as_bytes())?;
    f.flush()?;
    f.sync_all()?;
    Ok(())
}

fn validate_text_file(tmp_path: &Path) -> Result<()> {
    let text = fs::read_to_string(tmp_path)
        .with_context(|| format!("Temporary file {} is not valid UTF-8", tmp_path.display()))?;
    if text.is_empty() {
        bail!("Temporary output file is empty");
    }
    Ok(())
}

pub fn save_document(doc: &LoadedDocument, backup_dir: &Path, keep_last: usize) -> Result<PathBuf> {
    match doc.format {
        DocFormat::Txt | DocFormat::Markdown => {
            let content = doc.render_text_output();
            atomic_save_with_backup(
                &doc.path,
                backup_dir,
                keep_last,
                |tmp| write_text_file(tmp, &content),
                validate_text_file,
            )
        }
        DocFormat::Docx => {
            let docx_ref = doc
                .docx
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Missing DOCX structure in loaded document"))?;
            let replacements = doc.accepted_docx_replacements();
            atomic_save_with_backup(
                &doc.path,
                backup_dir,
                keep_last,
                |tmp| write_patched_docx(&doc.path, tmp, docx_ref, &replacements),
                validate_docx,
            )
        }
    }
}

pub fn save_document_as(doc: &LoadedDocument, dest: &Path) -> Result<()> {
    let dest_fmt = DocFormat::from_path(dest).map_err(|_| {
        anyhow::anyhow!(
            "Save As destination must keep the .{} extension",
            doc.format.default_extension()
        )
    })?;
    if dest_fmt != doc.format {
        bail!(
            "Cannot save a {} document as .{} — please use a .{} extension",
            doc.format.label(),
            dest_fmt.default_extension(),
            doc.format.default_extension()
        );
    }

    match doc.format {
        DocFormat::Txt | DocFormat::Markdown => {
            let content = doc.render_text_output();
            atomic_save_new_file(
                dest,
                |tmp| write_text_file(tmp, &content),
                validate_text_file,
            )
        }
        DocFormat::Docx => {
            let docx_ref = doc
                .docx
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Missing DOCX structure in loaded document"))?;
            let replacements = doc.accepted_docx_replacements();
            atomic_save_new_file(
                dest,
                |tmp| write_patched_docx(&doc.path, tmp, docx_ref, &replacements),
                validate_docx,
            )
        }
    }
}

#[derive(Debug)]
pub enum DocWorkerEvent {
    Progress {
        batch_index: usize,
        total_batches: usize,
        completed_items: usize,
        total_items: usize,
    },
    ItemDone {
        item_index: usize,
        res: Result<String, String>,
        auto_accept_unflagged: bool,
    },
    Finished {
        cancelled: bool,
    },
}

struct BusyResetGuard {
    flag: Arc<AtomicBool>,
    ctx: egui::Context,
}

impl Drop for BusyResetGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Relaxed);
        self.ctx.request_repaint();
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_review_worker(
    llm: Arc<LlmHandle>,
    work_items: Vec<(usize, usize, String)>,
    total_batches: usize,
    mode: PromptMode,
    preserve_markdown: bool,
    auto_accept_unflagged: bool,
    cancel: Arc<AtomicBool>,
    doc_review_busy: Arc<AtomicBool>,
    event_tx: Sender<DocWorkerEvent>,
    ctx: egui::Context,
) -> Result<JoinHandle<()>> {
    doc_review_busy.store(true, Ordering::Relaxed);
    let guard = BusyResetGuard {
        flag: doc_review_busy,
        ctx: ctx.clone(),
    };

    thread::Builder::new()
        .name("doc-review-worker".to_string())
        .spawn(move || {
            let _guard = guard;
            let total_items = work_items.len();
            let mut cancelled = false;

            for (done_idx, (batch_index, item_index, original)) in work_items.into_iter().enumerate()
            {
                if cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    break;
                }

                let _ = event_tx.send(DocWorkerEvent::Progress {
                    batch_index,
                    total_batches,
                    completed_items: done_idx,
                    total_items,
                });
                ctx.request_repaint();

                let res = if original.trim().is_empty() {
                    Ok(original)
                } else {
                    match llm.fix_text_with_options(
                        original,
                        mode,
                        preserve_markdown,
                        Some(Arc::clone(&cancel)),
                    ) {
                        Ok((fixed, _ms)) => Ok(fixed),
                        Err(err) => {
                            let msg = err.to_string();
                            if cancel.load(Ordering::Relaxed) || msg.contains("cancelled") {
                                cancelled = true;
                                break;
                            }
                            Err(msg)
                        }
                    }
                };

                if cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    break;
                }

                let _ = event_tx.send(DocWorkerEvent::ItemDone {
                    item_index,
                    res,
                    auto_accept_unflagged,
                });
                let _ = event_tx.send(DocWorkerEvent::Progress {
                    batch_index,
                    total_batches,
                    completed_items: done_idx + 1,
                    total_items,
                });
                ctx.request_repaint();
            }

            let _ = event_tx.send(DocWorkerEvent::Finished { cancelled });
            ctx.request_repaint();
        })
        .context("Failed to spawn document review worker thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_chunking_preserves_crlf_and_respects_accept_reject() {
        let raw = "First pargraph with typo.\r\n\r\nSecond pargraph stays.\r\n";
        let (pieces, mut items) = parse_txt_pieces(&raw.replace("\r\n", "\n"));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].original, "First pargraph with typo.");
        assert_eq!(items[1].original, "Second pargraph stays.");

        items[0].suggested = Some("First paragraph with typo.".to_string());
        items[0].decision = ReviewDecision::Accepted;
        items[1].suggested = Some("Second paragraph changed.".to_string());
        items[1].decision = ReviewDecision::Rejected;

        let batches = build_batches(&items);
        let doc = LoadedDocument {
            path: PathBuf::from("test.txt"),
            format: DocFormat::Txt,
            pieces,
            docx: None,
            items,
            batches,
            had_crlf: true,
        };
        assert_eq!(
            doc.render_text_output(),
            "First paragraph with typo.\r\n\r\nSecond pargraph stays.\r\n"
        );
    }

    #[test]
    fn markdown_chunking_preserves_frontmatter_and_fenced_code_blocks() {
        let md = "---\ntitle: My Doc\n---\n\n# Headingg\n\nIntro txt here.\n\n```rust\nlet wrd = \"dont touch\";\n```\n\nClosingg line.\n";
        let (pieces, mut items) = parse_markdown_pieces(md);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].original, "# Headingg");
        assert_eq!(items[1].original, "Intro txt here.");
        assert_eq!(items[2].original, "Closingg line.");

        items[0].suggested = Some("# Heading".to_string());
        items[0].decision = ReviewDecision::Accepted;
        items[2].suggested = Some("Closing line.".to_string());
        items[2].decision = ReviewDecision::Accepted;

        let batches = build_batches(&items);
        let doc = LoadedDocument {
            path: PathBuf::from("notes.md"),
            format: DocFormat::Markdown,
            pieces,
            docx: None,
            items,
            batches,
            had_crlf: false,
        };
        let rendered = doc.render_text_output();
        assert!(rendered.starts_with("---\ntitle: My Doc\n---\n\n# Heading\n"));
        assert!(rendered.contains("```rust\nlet wrd = \"dont touch\";\n```"));
        assert!(rendered.ends_with("Closing line.\n"));
    }

    #[test]
    fn word_diff_identifies_replacements_and_insertions() {
        let old = "The qucik brown fox";
        let new = "The quick brown red fox";
        let ops = compute_word_diff(old, new);
        assert_eq!(
            ops,
            vec![
                DiffOp::Equal("The ".to_string()),
                DiffOp::Delete("qucik".to_string()),
                DiffOp::Insert("quick".to_string()),
                DiffOp::Equal(" brown".to_string()),
                DiffOp::Insert(" red".to_string()),
                DiffOp::Equal(" fox".to_string()),
            ]
        );
        let reconstructed_old: String = ops
            .iter()
            .filter_map(|op| match op {
                DiffOp::Equal(s) | DiffOp::Delete(s) => Some(s.as_str()),
                DiffOp::Insert(_) => None,
            })
            .collect();
        let reconstructed_new: String = ops
            .iter()
            .filter_map(|op| match op {
                DiffOp::Equal(s) | DiffOp::Insert(s) => Some(s.as_str()),
                DiffOp::Delete(_) => None,
            })
            .collect();
        assert_eq!(reconstructed_old, old);
        assert_eq!(reconstructed_new, new);
    }

    #[test]
    fn length_deviation_flags_over_25_percent_only() {
        assert!(length_deviation_pct("short", "much longer text").is_none());
        assert!(length_deviation_pct("This is a normal sentence.", "This is a normal sentence!").is_none());
        let flagged_short = length_deviation_pct(
            "This is a moderately long sentence with several words.",
            "Too short.",
        );
        assert!(flagged_short.is_some_and(|p| p < -25.0));
        let flagged_long = length_deviation_pct(
            "This is a normal sentence.",
            "This is a much much much longer rewritten sentence with lots of extra words added.",
        );
        assert!(flagged_long.is_some_and(|p| p > 25.0));
    }

    #[test]
    fn rejects_unsupported_pdf_extension() {
        assert!(DocFormat::from_path(Path::new("report.pdf")).is_err());
        assert_eq!(DocFormat::from_path(Path::new("a.txt")).unwrap(), DocFormat::Txt);
        assert_eq!(DocFormat::from_path(Path::new("a.md")).unwrap(), DocFormat::Markdown);
        assert_eq!(DocFormat::from_path(Path::new("a.docx")).unwrap(), DocFormat::Docx);
    }

    #[test]
    fn save_document_as_rejects_mismatched_extension() {
        let dir = std::env::temp_dir().join(format!("kbllm_save_as_ext_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let (pieces, items) = parse_txt_pieces("Hello world.\n");
        let batches = build_batches(&items);
        let doc = LoadedDocument {
            path: dir.join("input.txt"),
            format: DocFormat::Txt,
            pieces,
            docx: None,
            items,
            batches,
            had_crlf: false,
        };
        let mismatched = dir.join("output.docx");
        assert!(save_document_as(&doc, &mismatched).is_err());
        assert!(!mismatched.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unchanged_suggestions_are_not_counted_or_batch_accepted() {
        let (pieces, mut items) = parse_txt_pieces("Clean sentence.\n\nBad speling.\n");
        items[0].suggested = Some("Clean sentence.".to_string());
        items[1].suggested = Some("Bad spelling.".to_string());
        let batches = build_batches(&items);
        let mut doc = LoadedDocument {
            path: PathBuf::from("test.txt"),
            format: DocFormat::Txt,
            pieces,
            docx: None,
            items,
            batches,
            had_crlf: false,
        };

        assert_eq!(doc.stats(), (2, 0, 1, 0));
        doc.accept_all_in_batch(0);
        assert_eq!(doc.stats(), (2, 1, 0, 0));
        assert_eq!(doc.items[0].decision, ReviewDecision::Pending);
        assert_eq!(doc.items[1].decision, ReviewDecision::Accepted);
    }
}
