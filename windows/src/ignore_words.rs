use crate::config::IgnoreWordEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtectedSpan {
    pub start: usize,
    pub end: usize,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn char_eq_ignore_case(a: char, b: char) -> bool {
    a.to_lowercase().eq(b.to_lowercase())
}

fn str_eq_ignore_case(a: &str, b: &str) -> bool {
    let mut ai = a.chars().flat_map(|c| c.to_lowercase());
    let mut bi = b.chars().flat_map(|c| c.to_lowercase());
    loop {
        match (ai.next(), bi.next()) {
            (Some(ac), Some(bc)) if ac == bc => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

pub fn find_protected_spans(text: &str, entries: &[IgnoreWordEntry]) -> Vec<ProtectedSpan> {
    if text.is_empty() || entries.is_empty() {
        return Vec::new();
    }

    let text_chars: Vec<(usize, char)> = text.char_indices().collect();
    let n = text_chars.len();
    let mut raw: Vec<ProtectedSpan> = Vec::new();

    for entry in entries {
        let trimmed = entry.word.trim();
        if trimmed.is_empty() {
            continue;
        }
        let needle_chars: Vec<char> = trimmed.chars().collect();
        let m = needle_chars.len();
        if m == 0 || m > n {
            continue;
        }

        let first_is_word = is_word_char(needle_chars[0]);
        let last_is_word = is_word_char(needle_chars[m - 1]);

        for ci in 0..=(n - m) {
            let matched = if entry.match_case {
                (0..m).all(|k| text_chars[ci + k].1 == needle_chars[k])
            } else {
                (0..m).all(|k| char_eq_ignore_case(text_chars[ci + k].1, needle_chars[k]))
            };
            if !matched {
                continue;
            }

            let left_ok = entry.ignore_word_boundary
                || !first_is_word
                || ci == 0
                || !is_word_char(text_chars[ci - 1].1);
            let right_ok = entry.ignore_word_boundary
                || !last_is_word
                || ci + m == n
                || !is_word_char(text_chars[ci + m].1);

            if !(left_ok && right_ok) {
                continue;
            }

            let mut start_ci = ci;
            let mut end_ci = ci + m;
            if entry.ignore_word_boundary {
                while start_ci > 0 && is_word_char(text_chars[start_ci - 1].1) {
                    start_ci -= 1;
                }
                while end_ci < n && is_word_char(text_chars[end_ci].1) {
                    end_ci += 1;
                }
            }

            let start_byte = text_chars[start_ci].0;
            let end_byte = if end_ci < n {
                text_chars[end_ci].0
            } else {
                text.len()
            };
            raw.push(ProtectedSpan {
                start: start_byte,
                end: end_byte,
            });
        }
    }

    if raw.is_empty() {
        return raw;
    }

    raw.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));
    let mut merged: Vec<ProtectedSpan> = Vec::with_capacity(raw.len());
    for span in raw {
        if let Some(last) = merged.last_mut() {
            if span.start < last.end {
                last.end = last.end.max(span.end);
                continue;
            }
        }
        merged.push(span);
    }
    merged
}

pub fn extract_unique_protected_terms<'a>(
    text: &'a str,
    spans: &[ProtectedSpan],
    max_terms: usize,
) -> Vec<&'a str> {
    let mut out: Vec<&'a str> = Vec::new();
    for s in spans {
        if out.len() >= max_terms {
            break;
        }
        let slice = &text[s.start..s.end];
        if !out.contains(&slice) {
            out.push(slice);
        }
    }
    out
}

pub fn format_prompt_hint(terms: &[&str]) -> Option<String> {
    if terms.is_empty() {
        return None;
    }
    let listed: Vec<String> = terms
        .iter()
        .filter_map(|t| serde_json::to_string(t).ok())
        .collect();
    if listed.is_empty() {
        return None;
    }
    Some(format!(
        "Keep these exact words/phrases unchanged (do not spell-check, re-case, or replace them): {}.",
        listed.join(", ")
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokKind {
    Word,
    Whitespace,
    Punct,
}

struct OrigToken<'a> {
    text: &'a str,
    protected: bool,
    kind: TokKind,
}

struct OutToken<'a> {
    text: &'a str,
    kind: TokKind,
}

fn tokenize_plain(s: &str) -> Vec<OutToken<'_>> {
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
            out.push(OutToken {
                text: &s[start..end],
                kind: TokKind::Whitespace,
            });
        } else if is_word_char(ch) {
            let mut end = start + ch.len_utf8();
            while let Some(&(next_i, next_c)) = iter.peek() {
                if is_word_char(next_c) {
                    end = next_i + next_c.len_utf8();
                    iter.next();
                } else {
                    break;
                }
            }
            out.push(OutToken {
                text: &s[start..end],
                kind: TokKind::Word,
            });
        } else {
            out.push(OutToken {
                text: &s[start..start + ch.len_utf8()],
                kind: TokKind::Punct,
            });
        }
    }
    out
}

fn tokenize_with_protected<'a>(s: &'a str, spans: &[ProtectedSpan]) -> Vec<OrigToken<'a>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for span in spans {
        if span.start > pos {
            for tok in tokenize_plain(&s[pos..span.start]) {
                out.push(OrigToken {
                    text: tok.text,
                    protected: false,
                    kind: tok.kind,
                });
            }
        }
        out.push(OrigToken {
            text: &s[span.start..span.end],
            protected: true,
            kind: TokKind::Word,
        });
        pos = span.end;
    }
    if pos < s.len() {
        for tok in tokenize_plain(&s[pos..]) {
            out.push(OrigToken {
                text: tok.text,
                protected: false,
                kind: tok.kind,
            });
        }
    }
    out
}

fn pair_score(o: &OrigToken<'_>, u: &OutToken<'_>) -> Option<u32> {
    if o.text == u.text {
        Some(if o.protected {
            8
        } else if o.kind == TokKind::Whitespace {
            2
        } else {
            6
        })
    } else if !o.protected && o.kind == TokKind::Word && u.kind == TokKind::Word {
        if str_eq_ignore_case(o.text, u.text) {
            Some(5)
        } else if normalized_similarity(o.text, u.text) >= 0.50 {
            Some(3)
        } else {
            None
        }
    } else if o.kind == TokKind::Whitespace && u.kind == TokKind::Whitespace {
        Some(1)
    } else {
        None
    }
}

fn char_levenshtein(a: &[char], b: &[char]) -> usize {
    let m = a.len();
    let n = b.len();
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];
    for (i, &ac) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &bc) in b.iter().enumerate() {
            let cost = usize::from(ac != bc);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

fn normalized_similarity(a: &str, b: &str) -> f32 {
    let ac: Vec<char> = a.chars().flat_map(|c| c.to_lowercase()).collect();
    let bc: Vec<char> = b.chars().flat_map(|c| c.to_lowercase()).collect();
    let max_len = ac.len().max(bc.len());
    if max_len == 0 {
        return 1.0;
    }
    let dist = char_levenshtein(&ac, &bc);
    1.0 - (dist as f32 / max_len as f32)
}

fn is_joinable_punct(tok: &str) -> bool {
    matches!(tok, "'" | "’" | "-" | "." | "_" | "/")
}

fn restore_gap(orig_gap: &[OrigToken<'_>], out_gap: &[OutToken<'_>], result: &mut String) {
    let protected_indices: Vec<usize> = orig_gap
        .iter()
        .enumerate()
        .filter_map(|(idx, tok)| tok.protected.then_some(idx))
        .collect();

    if protected_indices.is_empty() {
        for tok in out_gap {
            result.push_str(tok.text);
        }
        return;
    }

    let orig_word_positions: Vec<usize> = orig_gap
        .iter()
        .enumerate()
        .filter_map(|(idx, tok)| (tok.kind == TokKind::Word).then_some(idx))
        .collect();
    let out_word_positions: Vec<usize> = out_gap
        .iter()
        .enumerate()
        .filter_map(|(idx, tok)| (tok.kind == TokKind::Word).then_some(idx))
        .collect();

    // Pure single protected span: 1 protected token and 0 non-protected words in orig_gap,
    // matched against 0 or 1 word tokens in out_gap.
    if protected_indices.len() == 1
        && orig_word_positions.len() == 1
        && out_word_positions.len() <= 1
    {
        let prot_text = orig_gap[protected_indices[0]].text;
        if let Some(&w_idx) = out_word_positions.first() {
            for tok in &out_gap[..w_idx] {
                result.push_str(tok.text);
            }
            result.push_str(prot_text);
            for tok in &out_gap[w_idx + 1..] {
                result.push_str(tok.text);
            }
        } else {
            result.push_str(prot_text);
            for tok in out_gap {
                result.push_str(tok.text);
            }
        }
        return;
    }

    let mut out_parts: Vec<String> = out_gap.iter().map(|t| t.text.to_string()).collect();
    let mut used = vec![false; out_gap.len()];
    let mut unplaced: Vec<&str> = Vec::new();

    for &orig_idx in &protected_indices {
        let prot_text = orig_gap[orig_idx].text;

        // 1. If the exact protected text already exists verbatim in an unused span, lock it in.
        let mut exact_span: Option<(usize, usize)> = None;
        'outer: for a in 0..out_gap.len() {
            if used[a] {
                continue;
            }
            let mut combined = String::new();
            for b in a..out_gap.len().min(a + 6) {
                if used[b] {
                    break;
                }
                combined.push_str(out_gap[b].text);
                if combined == prot_text {
                    exact_span = Some((a, b));
                    break 'outer;
                }
                if combined.len() >= prot_text.len() {
                    break;
                }
            }
        }
        if let Some((a, b)) = exact_span {
            for flag in &mut used[a..=b] {
                *flag = true;
            }
            continue;
        }

        // 2. Score single and joinable multi-token word candidates in out_gap.
        let prot_has_ws = prot_text.chars().any(char::is_whitespace);
        let mut best: Option<(f32, usize, usize)> = None;

        for a in 0..out_gap.len() {
            if used[a] || out_gap[a].kind != TokKind::Word {
                continue;
            }
            let mut combined_raw = String::new();
            let mut combined_no_ws = String::new();
            let max_end = out_gap.len().min(a + 5);

            for b in a..max_end {
                if used[b] {
                    break;
                }
                match out_gap[b].kind {
                    TokKind::Word => {
                        combined_raw.push_str(out_gap[b].text);
                        combined_no_ws.push_str(out_gap[b].text);
                        let mut sim = normalized_similarity(prot_text, &combined_raw);
                        if !prot_has_ws && b > a {
                            sim = sim.max(normalized_similarity(prot_text, &combined_no_ws));
                        }
                        let is_better = match best {
                            None => true,
                            Some((best_sim, ba, bb)) => {
                                sim > best_sim + 0.001
                                    || ((sim - best_sim).abs() <= 0.001 && (b - a) < (bb - ba))
                            }
                        };
                        if is_better {
                            best = Some((sim, a, b));
                        }
                    }
                    TokKind::Whitespace => {
                        combined_raw.push_str(out_gap[b].text);
                    }
                    TokKind::Punct => {
                        if is_joinable_punct(out_gap[b].text) {
                            combined_raw.push_str(out_gap[b].text);
                            combined_no_ws.push_str(out_gap[b].text);
                        } else {
                            break;
                        }
                    }
                }
            }
        }

        let p_ord = orig_word_positions
            .iter()
            .position(|&idx| idx == orig_idx)
            .unwrap_or(0);

        let chosen = match best {
            Some((sim, a, b)) if sim >= 0.35 => Some((a, b)),
            _ if orig_word_positions.len() == out_word_positions.len() => {
                let slot = out_word_positions[p_ord];
                (!used[slot]).then_some((slot, slot))
            }
            _ => None,
        };

        if let Some((a, b)) = chosen {
            out_parts[a] = prot_text.to_string();
            for part in &mut out_parts[(a + 1)..=b] {
                part.clear();
            }
            for flag in &mut used[a..=b] {
                *flag = true;
            }
        } else {
            unplaced.push(prot_text);
        }
    }

    for part in out_parts {
        result.push_str(&part);
    }
    for term in unplaced {
        result.push_str(term);
    }
}

pub fn restore_protected_spans(
    original: &str,
    llm_output: &str,
    spans: &[ProtectedSpan],
) -> String {
    if spans.is_empty() || original == llm_output {
        return llm_output.to_string();
    }

    let orig = tokenize_with_protected(original, spans);
    let out = tokenize_plain(llm_output);
    let m = orig.len();
    let n = out.len();

    if m == 0 {
        return llm_output.to_string();
    }
    if m.saturating_mul(n) > 250_000 {
        return llm_output.to_string();
    }

    let cols = n + 1;
    let mut dp = vec![0u32; (m + 1) * cols];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            let skip = dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)]);
            let best = match pair_score(&orig[i], &out[j]) {
                Some(score) => skip.max(dp[(i + 1) * cols + (j + 1)] + score),
                None => skip,
            };
            dp[i * cols + j] = best;
        }
    }

    let mut aligned: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < m && j < n {
        // When a whitespace token follows a protected word, let it attach as late as possible
        // (to the right neighbor) so multi-word splits of the protected word stay in one gap.
        let right_attach_ws =
            orig[i].kind == TokKind::Whitespace && i > 0 && orig[i - 1].protected;
        if right_attach_ws && dp[i * cols + j] == dp[i * cols + (j + 1)] {
            j += 1;
            continue;
        }
        if let Some(score) = pair_score(&orig[i], &out[j]) {
            if dp[i * cols + j] == dp[(i + 1) * cols + (j + 1)] + score {
                aligned.push((i, j));
                i += 1;
                j += 1;
                continue;
            }
        }
        if dp[(i + 1) * cols + j] >= dp[i * cols + (j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }

    let mut result = String::with_capacity(llm_output.len() + 16);
    let mut oi_start = 0usize;
    let mut oj_start = 0usize;

    for (oi, oj) in aligned {
        restore_gap(&orig[oi_start..oi], &out[oj_start..oj], &mut result);
        result.push_str(out[oj].text);
        oi_start = oi + 1;
        oj_start = oj + 1;
    }
    restore_gap(&orig[oi_start..], &out[oj_start..], &mut result);

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(word: &str, match_case: bool, ignore_word_boundary: bool) -> IgnoreWordEntry {
        IgnoreWordEntry {
            word: word.to_string(),
            match_case,
            ignore_word_boundary,
        }
    }

    fn apply_protect(original: &str, llm_output: &str, entries: &[IgnoreWordEntry]) -> String {
        let spans = find_protected_spans(original, entries);
        restore_protected_spans(original, llm_output, &spans)
    }

    #[test]
    fn protects_discord_name_and_slang_while_keeping_grammar_fixes() {
        let entries = [entry("shadowfox", false, false), entry("corpa", false, false)];
        assert_eq!(
            apply_protect(
                "hey shadowfox how r u",
                "Hey Shadow, how are you?",
                &entries
            ),
            "Hey shadowfox, how are you?"
        );
        assert_eq!(
            apply_protect(
                "this corpa move is bad",
                "This Corp move is bad.",
                &entries
            ),
            "This corpa move is bad."
        );
    }

    #[test]
    fn match_case_option_respects_and_preserves_exact_casing() {
        let case_sensitive = [entry("US", true, false)];
        assert_eq!(
            apply_protect(
                "talk to us about US policy",
                "Talk to them about USA policy.",
                &case_sensitive
            ),
            "Talk to them about US policy."
        );
        // "us" did not match "US" when match_case=true; when match_case=false it keeps original case
        let case_insensitive = [entry("shadowfox", false, false)];
        assert_eq!(
            apply_protect(
                "Shadowfox and SHADOWFOX are here",
                "Shadow and Shadow are here.",
                &case_insensitive
            ),
            "Shadowfox and SHADOWFOX are here."
        );
    }

    #[test]
    fn word_boundary_option_distinguishes_whole_word_vs_compound_words() {
        let whole_only = [entry("corpa", false, false)];
        assert_eq!(
            apply_protect(
                "incorporate @corpa into corpa. and corpa's plan",
                "Incorporate @Corp into Corp. and Corp's plan.",
                &whole_only
            ),
            "Incorporate @corpa into corpa. and corpa's plan."
        );
        assert!(find_protected_spans("incorporate", &whole_only).is_empty());

        let substring = [entry("corpa", false, true)];
        assert_eq!(
            apply_protect(
                "megacorpa and corpas are wild",
                "Megacorp and corps are wild.",
                &substring
            ),
            "megacorpa and corpas are wild."
        );
    }

    #[test]
    fn handles_split_words_mixed_rewrites_and_possessives() {
        let entries = [entry("shadowfox", false, false)];
        assert_eq!(
            apply_protect(
                "ask shadowfox pls",
                "Ask shadow fox, please.",
                &entries
            ),
            "Ask shadowfox, please."
        );
        assert_eq!(
            apply_protect(
                "idk shadowfox maybe",
                "I don't know, Shadow, maybe.",
                &entries
            ),
            "I don't know, shadowfox, maybe."
        );
        assert_eq!(
            apply_protect(
                "shadowfox's car is broke",
                "Shadow's car is broken.",
                &entries
            ),
            "shadowfox's car is broken."
        );
    }

    #[test]
    fn handles_multi_word_phrases_hyphenated_names_and_synonym_swaps() {
        let entries = [
            entry("Corpa Inc", false, false),
            entry("O'Brien", false, false),
            entry("shadowfox", false, false),
        ];
        assert_eq!(
            apply_protect("call Corpa Inc", "Call Corp Inc.", &entries),
            "Call Corpa Inc."
        );
        assert_eq!(
            apply_protect("ask O'Brien pls", "Ask O'Brian, please.", &entries),
            "Ask O'Brien, please."
        );
        assert_eq!(
            apply_protect("hey shadowfox", "Hey friend.", &entries),
            "Hey shadowfox."
        );
    }

    #[test]
    fn handles_unicode_empty_and_no_match_safely() {
        let entries = [entry("shadowfox", false, false), entry("İzmir", false, false)];
        assert_eq!(
            apply_protect("go to İzmir now", "Go to Izmir now.", &entries),
            "Go to İzmir now."
        );
        assert_eq!(
            apply_protect("helo wrold", "Hello world.", &entries),
            "Hello world."
        );
        assert_eq!(
            apply_protect("helo shadowfox", "Hello shadowfox.", &[]),
            "Hello shadowfox."
        );
    }

    #[test]
    fn format_prompt_hint_only_emits_when_terms_present() {
        assert_eq!(format_prompt_hint(&[]), None);
        let hint = format_prompt_hint(&["shadowfox", "corpa"]).unwrap();
        assert!(hint.contains("\"shadowfox\""));
        assert!(hint.contains("\"corpa\""));
    }
}
