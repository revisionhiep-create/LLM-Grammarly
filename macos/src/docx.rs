use anyhow::{anyhow, bail, Context, Result};
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::doc_review::{compute_word_diff, DiffOp};

#[derive(Debug, Clone)]
pub struct DocxRunSpan {
    pub t_index: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub rpr_key: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct DocxSegment {
    pub para_index: usize,
    pub seg_index: usize,
    pub text: String,
    pub runs: Vec<DocxRunSpan>,
    pub has_mixed_formatting: bool,
}

#[derive(Debug, Clone)]
pub struct DocxDocument {
    pub segments: Vec<DocxSegment>,
}

fn is_w_tag(name: &[u8], expected_local: &[u8]) -> bool {
    if let Some(rest) = name.strip_prefix(b"w:") {
        rest == expected_local
    } else {
        name == expected_local
    }
}

fn make_run_key(hyperlink_depth: usize, ins_depth: usize, rpr: &[u8]) -> Vec<u8> {
    let mut key = format!("h{hyperlink_depth}:i{ins_depth}\0").into_bytes();
    key.extend_from_slice(rpr);
    key
}

fn attr_fld_char_type(e: &BytesStart<'_>) -> Option<Vec<u8>> {
    for attr in e.attributes().flatten() {
        let key = attr.key.as_ref();
        if key == b"w:fldCharType" || key == b"fldCharType" {
            return Some(attr.value.into_owned());
        }
    }
    None
}

struct ActivePara {
    para_index: usize,
    next_t_index: usize,
    next_seg_index: usize,
    fld_char_depth: usize,
    seg_text: String,
    seg_runs: Vec<DocxRunSpan>,
}

impl ActivePara {
    fn new(para_index: usize) -> Self {
        Self {
            para_index,
            next_t_index: 0,
            next_seg_index: 0,
            fld_char_depth: 0,
            seg_text: String::new(),
            seg_runs: Vec::new(),
        }
    }

    fn flush_segment(&mut self, out: &mut Vec<DocxSegment>) {
        if self.seg_runs.is_empty() {
            self.seg_text.clear();
            return;
        }
        let text = std::mem::take(&mut self.seg_text);
        let runs = std::mem::take(&mut self.seg_runs);
        let seg_index = self.next_seg_index;
        self.next_seg_index += 1;

        if text.trim().is_empty() {
            return;
        }
        let first_key = runs.first().map(|r| r.rpr_key.as_slice()).unwrap_or(&[]);
        let has_mixed_formatting = runs.iter().any(|r| r.rpr_key.as_slice() != first_key);
        out.push(DocxSegment {
            para_index: self.para_index,
            seg_index,
            text,
            runs,
            has_mixed_formatting,
        });
    }
}

pub fn parse_document_xml(xml: &[u8]) -> Result<DocxDocument> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);

    let mut buf = Vec::new();
    let mut segments = Vec::new();
    let mut next_para_index = 0usize;
    let mut para_stack: Vec<ActivePara> = Vec::new();

    let mut del_depth = 0usize;
    let mut fld_simple_depth = 0usize;
    let mut hyperlink_depth = 0usize;
    let mut ins_depth = 0usize;
    let mut in_run = false;
    let mut rpr_depth = 0usize;
    let mut current_rpr: Vec<u8> = Vec::new();
    let mut active_t_index: Option<usize> = None;
    let mut active_t_buf = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let name_bytes = name.as_ref();

                if rpr_depth > 0 {
                    rpr_depth += 1;
                    current_rpr.push(b'<');
                    current_rpr.extend_from_slice(e.as_ref());
                    current_rpr.push(b'>');
                } else if is_w_tag(name_bytes, b"p") {
                    let idx = next_para_index;
                    next_para_index += 1;
                    para_stack.push(ActivePara::new(idx));
                } else if is_w_tag(name_bytes, b"del") || is_w_tag(name_bytes, b"delText") {
                    if let Some(p) = para_stack.last_mut() {
                        p.flush_segment(&mut segments);
                    }
                    del_depth += 1;
                } else if is_w_tag(name_bytes, b"fldSimple") {
                    if let Some(p) = para_stack.last_mut() {
                        p.flush_segment(&mut segments);
                    }
                    fld_simple_depth += 1;
                } else if is_w_tag(name_bytes, b"fldChar") {
                    if let Some(p) = para_stack.last_mut() {
                        p.flush_segment(&mut segments);
                        match attr_fld_char_type(&e).as_deref() {
                            Some(b"begin") => p.fld_char_depth += 1,
                            Some(b"end") => p.fld_char_depth = p.fld_char_depth.saturating_sub(1),
                            _ => {}
                        }
                    }
                } else if is_w_tag(name_bytes, b"hyperlink") {
                    hyperlink_depth += 1;
                } else if is_w_tag(name_bytes, b"ins") {
                    ins_depth += 1;
                } else if is_w_tag(name_bytes, b"r") {
                    in_run = true;
                    current_rpr.clear();
                } else if in_run && is_w_tag(name_bytes, b"rPr") {
                    rpr_depth = 1;
                    current_rpr.clear();
                } else if is_w_tag(name_bytes, b"t") {
                    if let Some(p) = para_stack.last_mut() {
                        let t_idx = p.next_t_index;
                        p.next_t_index += 1;
                        if del_depth == 0 && fld_simple_depth == 0 && p.fld_char_depth == 0 {
                            active_t_index = Some(t_idx);
                            active_t_buf.clear();
                        }
                    }
                } else if is_w_tag(name_bytes, b"tab")
                    || is_w_tag(name_bytes, b"br")
                    || is_w_tag(name_bytes, b"cr")
                    || is_w_tag(name_bytes, b"drawing")
                    || is_w_tag(name_bytes, b"pict")
                    || is_w_tag(name_bytes, b"object")
                    || is_w_tag(name_bytes, b"footnoteReference")
                    || is_w_tag(name_bytes, b"endnoteReference")
                {
                    if let Some(p) = para_stack.last_mut() {
                        p.flush_segment(&mut segments);
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                let name = e.name();
                let name_bytes = name.as_ref();

                if rpr_depth > 0 {
                    current_rpr.push(b'<');
                    current_rpr.extend_from_slice(e.as_ref());
                    current_rpr.extend_from_slice(b"/>");
                } else if is_w_tag(name_bytes, b"fldChar") {
                    if let Some(p) = para_stack.last_mut() {
                        p.flush_segment(&mut segments);
                        match attr_fld_char_type(&e).as_deref() {
                            Some(b"begin") => p.fld_char_depth += 1,
                            Some(b"end") => p.fld_char_depth = p.fld_char_depth.saturating_sub(1),
                            _ => {}
                        }
                    }
                } else if is_w_tag(name_bytes, b"t") {
                    if let Some(p) = para_stack.last_mut() {
                        let t_idx = p.next_t_index;
                        p.next_t_index += 1;
                        if del_depth == 0 && fld_simple_depth == 0 && p.fld_char_depth == 0 {
                            let pos = p.seg_text.len();
                            p.seg_runs.push(DocxRunSpan {
                                t_index: t_idx,
                                byte_start: pos,
                                byte_end: pos,
                                rpr_key: make_run_key(hyperlink_depth, ins_depth, &current_rpr),
                            });
                        }
                    }
                } else if is_w_tag(name_bytes, b"tab")
                    || is_w_tag(name_bytes, b"br")
                    || is_w_tag(name_bytes, b"cr")
                    || is_w_tag(name_bytes, b"drawing")
                    || is_w_tag(name_bytes, b"pict")
                    || is_w_tag(name_bytes, b"object")
                    || is_w_tag(name_bytes, b"footnoteReference")
                    || is_w_tag(name_bytes, b"endnoteReference")
                {
                    if let Some(p) = para_stack.last_mut() {
                        p.flush_segment(&mut segments);
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if rpr_depth > 0 {
                    current_rpr.extend_from_slice(e.as_ref());
                } else if active_t_index.is_some() {
                    let decoded = e
                        .unescape()
                        .map_err(|err| anyhow!("Failed to decode XML text in DOCX: {err}"))?;
                    active_t_buf.push_str(&decoded);
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let name_bytes = name.as_ref();

                if rpr_depth > 0 {
                    rpr_depth -= 1;
                    if rpr_depth > 0 {
                        current_rpr.extend_from_slice(b"</");
                        current_rpr.extend_from_slice(name_bytes);
                        current_rpr.push(b'>');
                    }
                } else if is_w_tag(name_bytes, b"t") {
                    if let Some(t_idx) = active_t_index.take() {
                        if let Some(p) = para_stack.last_mut() {
                            let start = p.seg_text.len();
                            p.seg_text.push_str(&active_t_buf);
                            let end = p.seg_text.len();
                            p.seg_runs.push(DocxRunSpan {
                                t_index: t_idx,
                                byte_start: start,
                                byte_end: end,
                                rpr_key: make_run_key(hyperlink_depth, ins_depth, &current_rpr),
                            });
                        }
                        active_t_buf.clear();
                    }
                } else if is_w_tag(name_bytes, b"r") {
                    in_run = false;
                    current_rpr.clear();
                } else if is_w_tag(name_bytes, b"hyperlink") {
                    hyperlink_depth = hyperlink_depth.saturating_sub(1);
                } else if is_w_tag(name_bytes, b"ins") {
                    ins_depth = ins_depth.saturating_sub(1);
                } else if is_w_tag(name_bytes, b"del") || is_w_tag(name_bytes, b"delText") {
                    del_depth = del_depth.saturating_sub(1);
                } else if is_w_tag(name_bytes, b"fldSimple") {
                    fld_simple_depth = fld_simple_depth.saturating_sub(1);
                } else if is_w_tag(name_bytes, b"p") {
                    if let Some(mut p) = para_stack.pop() {
                        p.flush_segment(&mut segments);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => bail!("Failed to parse word/document.xml: {err}"),
            _ => {}
        }
        buf.clear();
    }

    Ok(DocxDocument { segments })
}

pub fn read_docx(path: &Path) -> Result<DocxDocument> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open DOCX file {}", path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("{} is not a valid ZIP/DOCX archive", path.display()))?;
    let mut doc_xml = archive
        .by_name("word/document.xml")
        .context("DOCX archive is missing word/document.xml")?;
    let mut xml_bytes = Vec::new();
    doc_xml.read_to_end(&mut xml_bytes)?;
    parse_document_xml(&xml_bytes)
}

fn owner_run_at(runs: &[DocxRunSpan], byte_pos: usize) -> usize {
    for (i, r) in runs.iter().enumerate() {
        if byte_pos < r.byte_end {
            return i;
        }
    }
    runs.len().saturating_sub(1)
}

fn compute_run_texts(seg: &DocxSegment, new_text: &str) -> Vec<(usize, String)> {
    if seg.runs.is_empty() {
        return Vec::new();
    }
    if !seg.has_mixed_formatting || seg.runs.len() == 1 {
        let mut out = Vec::with_capacity(seg.runs.len());
        out.push((seg.runs[0].t_index, new_text.to_string()));
        for run in &seg.runs[1..] {
            out.push((run.t_index, String::new()));
        }
        return out;
    }

    let diff = compute_word_diff(&seg.text, new_text);
    let mut run_outputs = vec![String::new(); seg.runs.len()];
    let mut orig_cursor = 0usize;
    let mut last_del_run: Option<usize> = None;

    for op in diff {
        match op {
            DiffOp::Equal(eq) => {
                last_del_run = None;
                let end = (orig_cursor + eq.len()).min(seg.text.len());
                for (idx, run) in seg.runs.iter().enumerate() {
                    let lo = orig_cursor.max(run.byte_start);
                    let hi = end.min(run.byte_end);
                    if lo < hi {
                        run_outputs[idx].push_str(&seg.text[lo..hi]);
                    }
                }
                orig_cursor = end;
            }
            DiffOp::Delete(del) => {
                if last_del_run.is_none() {
                    last_del_run = Some(owner_run_at(&seg.runs, orig_cursor));
                }
                orig_cursor = (orig_cursor + del.len()).min(seg.text.len());
            }
            DiffOp::Insert(ins) => {
                let target = last_del_run.take().unwrap_or_else(|| {
                    if orig_cursor == 0 {
                        0
                    } else {
                        owner_run_at(&seg.runs, orig_cursor - 1)
                    }
                });
                run_outputs[target].push_str(&ins);
            }
        }
    }

    seg.runs
        .iter()
        .zip(run_outputs)
        .map(|(r, txt)| (r.t_index, txt))
        .collect()
}

fn start_tag_with_preserve_space(e: &BytesStart<'_>) -> Result<BytesStart<'static>> {
    let tag_name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let mut out = BytesStart::new(tag_name);
    let mut has_xml_space = false;
    for attr in e.attributes() {
        let attr = attr.map_err(|err| anyhow!("Invalid XML attribute in <w:t>: {err}"))?;
        if attr.key.as_ref() == b"xml:space" {
            has_xml_space = true;
            out.push_attribute(("xml:space", "preserve"));
        } else {
            out.push_attribute(attr);
        }
    }
    if !has_xml_space {
        out.push_attribute(("xml:space", "preserve"));
    }
    Ok(out.into_owned())
}

pub fn patch_document_xml(
    xml: &[u8],
    doc: &DocxDocument,
    replacements: &HashMap<(usize, usize), String>,
) -> Result<Vec<u8>> {
    let mut run_map: HashMap<(usize, usize), String> = HashMap::new();
    for seg in &doc.segments {
        if let Some(new_text) = replacements.get(&(seg.para_index, seg.seg_index)) {
            if new_text != &seg.text {
                for (t_index, run_text) in compute_run_texts(seg, new_text) {
                    run_map.insert((seg.para_index, t_index), run_text);
                }
            }
        }
    }

    if run_map.is_empty() {
        return Ok(xml.to_vec());
    }

    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Cursor::new(Vec::with_capacity(xml.len() + 256)));

    let mut buf = Vec::new();
    let mut next_para_index = 0usize;
    let mut para_stack: Vec<(usize, usize)> = Vec::new();
    let mut skipping_t_contents = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name_bytes = e.name().as_ref().to_vec();
                if is_w_tag(&name_bytes, b"p") {
                    let idx = next_para_index;
                    next_para_index += 1;
                    para_stack.push((idx, 0));
                    writer.write_event(Event::Start(e))?;
                } else if is_w_tag(&name_bytes, b"t") {
                    let replacement = if let Some((para_idx, next_t)) = para_stack.last_mut() {
                        let t_idx = *next_t;
                        *next_t += 1;
                        run_map.get(&(*para_idx, t_idx))
                    } else {
                        None
                    };
                    if let Some(new_text) = replacement {
                        let start = if !new_text.is_empty() {
                            start_tag_with_preserve_space(&e)?
                        } else {
                            e.into_owned()
                        };
                        writer.write_event(Event::Start(start))?;
                        if !new_text.is_empty() {
                            writer.write_event(Event::Text(BytesText::new(new_text)))?;
                        }
                        skipping_t_contents = true;
                    } else {
                        writer.write_event(Event::Start(e))?;
                    }
                } else if !skipping_t_contents {
                    writer.write_event(Event::Start(e))?;
                }
            }
            Ok(Event::Empty(e)) => {
                let name_bytes = e.name().as_ref().to_vec();
                if is_w_tag(&name_bytes, b"t") {
                    let replacement = if let Some((para_idx, next_t)) = para_stack.last_mut() {
                        let t_idx = *next_t;
                        *next_t += 1;
                        run_map.get(&(*para_idx, t_idx))
                    } else {
                        None
                    };
                    if let Some(new_text) = replacement {
                        if new_text.is_empty() {
                            writer.write_event(Event::Empty(e))?;
                        } else {
                            let tag_name = String::from_utf8_lossy(&name_bytes).into_owned();
                            let start = start_tag_with_preserve_space(&e)?;
                            writer.write_event(Event::Start(start))?;
                            writer.write_event(Event::Text(BytesText::new(new_text)))?;
                            writer.write_event(Event::End(BytesEnd::new(tag_name)))?;
                        }
                    } else {
                        writer.write_event(Event::Empty(e))?;
                    }
                } else if !skipping_t_contents {
                    writer.write_event(Event::Empty(e))?;
                }
            }
            Ok(Event::End(e)) => {
                let name_bytes = e.name().as_ref().to_vec();
                if is_w_tag(&name_bytes, b"t") && skipping_t_contents {
                    skipping_t_contents = false;
                    writer.write_event(Event::End(e))?;
                } else {
                    if is_w_tag(&name_bytes, b"p") {
                        para_stack.pop();
                    }
                    if !skipping_t_contents {
                        writer.write_event(Event::End(e))?;
                    }
                }
            }
            Ok(Event::Text(e)) => {
                if !skipping_t_contents {
                    writer.write_event(Event::Text(e))?;
                }
            }
            Ok(Event::CData(e)) => {
                if !skipping_t_contents {
                    writer.write_event(Event::CData(e))?;
                }
            }
            Ok(Event::Comment(e)) => {
                if !skipping_t_contents {
                    writer.write_event(Event::Comment(e))?;
                }
            }
            Ok(Event::Decl(e)) => writer.write_event(Event::Decl(e))?,
            Ok(Event::PI(e)) => writer.write_event(Event::PI(e))?,
            Ok(Event::DocType(e)) => writer.write_event(Event::DocType(e))?,
            Ok(Event::Eof) => break,
            Err(err) => bail!("Failed while patching word/document.xml: {err}"),
        }
        buf.clear();
    }

    Ok(writer.into_inner().into_inner())
}

pub fn write_patched_docx(
    src: &Path,
    dest_tmp: &Path,
    doc: &DocxDocument,
    replacements: &HashMap<(usize, usize), String>,
) -> Result<()> {
    let src_file = File::open(src)
        .with_context(|| format!("Failed to open source DOCX {}", src.display()))?;
    let mut archive = ZipArchive::new(src_file)
        .with_context(|| format!("Failed to read DOCX archive {}", src.display()))?;

    let mut original_xml = Vec::new();
    {
        let mut xml_entry = archive
            .by_name("word/document.xml")
            .context("DOCX archive missing word/document.xml")?;
        xml_entry.read_to_end(&mut original_xml)?;
    }

    let patched_xml = patch_document_xml(&original_xml, doc, replacements)?;

    let out_file = File::create(dest_tmp)
        .with_context(|| format!("Failed to create temporary DOCX {}", dest_tmp.display()))?;
    let mut writer = ZipWriter::new(out_file);
    let xml_options =
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        if entry.name()?.as_ref() == "word/document.xml" {
            drop(entry);
            writer.start_file("word/document.xml", xml_options)?;
            writer.write_all(&patched_xml)?;
        } else {
            writer.raw_copy_file(entry)?;
        }
    }

    let mut out_file = writer.finish()?;
    out_file.flush()?;
    out_file.sync_all()?;
    Ok(())
}

pub fn validate_docx(path: &Path) -> Result<()> {
    let file = File::open(path)
        .with_context(|| format!("Failed to open DOCX for validation {}", path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("Validated DOCX is not a valid ZIP archive: {}", path.display()))?;
    let mut xml_entry = archive
        .by_name("word/document.xml")
        .context("Validated DOCX is missing word/document.xml")?;
    let mut xml = Vec::new();
    xml_entry.read_to_end(&mut xml)?;
    if xml.is_empty() {
        bail!("Validated word/document.xml is empty");
    }

    let mut reader = Reader::from_reader(xml.as_slice());
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(err) => bail!("Validated word/document.xml is malformed XML: {err}"),
        }
        buf.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DOCX_TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn build_sample_docx(path: &Path) {
        let doc_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p>
      <w:r><w:t>My </w:t></w:r>
      <w:proofErr w:type="spellStart"/>
      <w:r><w:t>gar</w:t></w:r>
      <w:r><w:t>mma</w:t></w:r>
      <w:proofErr w:type="spellEnd"/>
      <w:r><w:t xml:space="preserve"> is bad.</w:t></w:r>
    </w:p>
    <w:p>
      <w:r><w:t xml:space="preserve">The </w:t></w:r>
      <w:r><w:rPr><w:b/></w:rPr><w:t>qucik</w:t></w:r>
      <w:r><w:t xml:space="preserve"> brown fox.</w:t></w:r>
    </w:p>
    <w:tbl>
      <w:tr>
        <w:tc>
          <w:p><w:r><w:t>Tabel cel text.</w:t></w:r></w:p>
        </w:tc>
      </w:tr>
    </w:tbl>
    <w:p>
      <w:r><w:fldChar w:fldCharType="begin"/></w:r>
      <w:r><w:instrText> PAGE </w:instrText></w:r>
      <w:r><w:fldChar w:fldCharType="separate"/></w:r>
      <w:r><w:t>1</w:t></w:r>
      <w:r><w:fldChar w:fldCharType="end"/></w:r>
      <w:del w:id="1"><w:r><w:delText>deleted txt</w:delText></w:r></w:del>
    </w:p>
  </w:body>
</w:document>"#;

        let file = File::create(path).unwrap();
        let mut zw = ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        zw.start_file("[Content_Types].xml", opts).unwrap();
        zw.write_all(b"<Types/>").unwrap();

        zw.start_file("word/styles.xml", opts).unwrap();
        zw.write_all(b"<w:styles>keep-styles</w:styles>").unwrap();

        zw.start_file("word/media/image1.png", opts).unwrap();
        zw.write_all(&[0x89, b'P', b'N', b'G', 1, 2, 3, 4]).unwrap();

        zw.start_file("word/document.xml", opts).unwrap();
        zw.write_all(doc_xml.as_bytes()).unwrap();
        zw.finish().unwrap();
    }

    #[test]
    fn docx_patches_fragmented_mixed_and_table_runs_while_preserving_assets_and_fields() {
        let seq = DOCX_TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "kbllm_docx_test_{}_{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let src = dir.join("sample.docx");
        let dst = dir.join("patched.docx");
        build_sample_docx(&src);

        let doc = read_docx(&src).unwrap();
        assert_eq!(doc.segments.len(), 3);
        assert_eq!(doc.segments[0].text, "My garmma is bad.");
        assert!(!doc.segments[0].has_mixed_formatting);
        assert_eq!(doc.segments[1].text, "The qucik brown fox.");
        assert!(doc.segments[1].has_mixed_formatting);
        assert_eq!(doc.segments[2].text, "Tabel cel text.");

        let mut replacements = HashMap::new();
        replacements.insert(
            (doc.segments[0].para_index, doc.segments[0].seg_index),
            "My grammar is bad.".to_string(),
        );
        replacements.insert(
            (doc.segments[1].para_index, doc.segments[1].seg_index),
            "The quick brown fox.".to_string(),
        );
        replacements.insert(
            (doc.segments[2].para_index, doc.segments[2].seg_index),
            "Table cell text.".to_string(),
        );

        write_patched_docx(&src, &dst, &doc, &replacements).unwrap();
        validate_docx(&dst).unwrap();

        let reread = read_docx(&dst).unwrap();
        assert_eq!(reread.segments.len(), 3);
        assert_eq!(reread.segments[0].text, "My grammar is bad.");
        assert_eq!(reread.segments[1].text, "The quick brown fox.");
        assert_eq!(reread.segments[2].text, "Table cell text.");

        let mut archive = ZipArchive::new(File::open(&dst).unwrap()).unwrap();
        let mut img = Vec::new();
        archive
            .by_name("word/media/image1.png")
            .unwrap()
            .read_to_end(&mut img)
            .unwrap();
        assert_eq!(img, vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4]);

        let mut styles = String::new();
        archive
            .by_name("word/styles.xml")
            .unwrap()
            .read_to_string(&mut styles)
            .unwrap();
        assert_eq!(styles, "<w:styles>keep-styles</w:styles>");

        let mut patched_xml = String::new();
        archive
            .by_name("word/document.xml")
            .unwrap()
            .read_to_string(&mut patched_xml)
            .unwrap();
        assert!(patched_xml.contains("<w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">quick</w:t>"));
        assert!(patched_xml.contains("<w:delText>deleted txt</w:delText>"));
        assert!(patched_xml.contains("<w:instrText> PAGE </w:instrText>"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn docx_preserves_hyperlink_boundaries_even_without_rpr() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t xml:space="preserve">See </w:t></w:r><w:hyperlink><w:r><w:t>exampel</w:t></w:r></w:hyperlink><w:r><w:t xml:space="preserve"> now.</w:t></w:r></w:p>
  </w:body>
</w:document>"#;

        let doc = parse_document_xml(xml).unwrap();
        assert_eq!(doc.segments.len(), 1);
        assert_eq!(doc.segments[0].text, "See exampel now.");
        assert!(doc.segments[0].has_mixed_formatting);

        let mut replacements = HashMap::new();
        replacements.insert(
            (doc.segments[0].para_index, doc.segments[0].seg_index),
            "See example now.".to_string(),
        );

        let patched = patch_document_xml(xml, &doc, &replacements).unwrap();
        let patched_str = String::from_utf8(patched).unwrap();
        assert!(patched_str.contains("<w:hyperlink><w:r><w:t xml:space=\"preserve\">example</w:t></w:r></w:hyperlink>"));
        assert!(patched_str.contains("<w:r><w:t xml:space=\"preserve\">See </w:t></w:r><w:hyperlink>"));
        assert!(patched_str.contains("</w:hyperlink><w:r><w:t xml:space=\"preserve\"> now.</w:t></w:r>"));
    }
}
