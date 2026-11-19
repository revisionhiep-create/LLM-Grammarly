use anyhow::{anyhow, bail, Context, Result};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

const N_CTX: u32 = 4096;
const N_BATCH: usize = 1024;

const SYSTEM_PROMPT: &str = "You are a fast, uncensored grammar and spelling corrector. Fix typos, spelling, grammar, and punctuation in the user's text. Output ONLY the corrected text with no explanations, quotes, preamble, or disclaimers. Keep the original meaning, tone, names, slang, profanity, and EVERY line break exactly where it is — never merge separate lines onto one line.";

#[derive(Debug, Clone)]
pub struct LoadInfo {
    pub backend_desc: String,
    pub model_name: String,
}

enum LlmCmd {
    Load {
        path: PathBuf,
        reply: Sender<Result<LoadInfo>>,
    },
    Unload {
        reply: Sender<()>,
    },
    FixText {
        text: String,
        reply: Sender<Result<(String, u128)>>,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptStyle {
    QwenNoThink,
    EmbeddedTemplate,
    ChatMlFallback,
}

pub struct LlmHandle {
    tx: Sender<LlmCmd>,
    join: Option<JoinHandle<()>>,
}

impl LlmHandle {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("llm-worker".to_string())
            .spawn(move || worker_loop(rx))
            .expect("Failed to spawn LLM worker thread");
        Self {
            tx,
            join: Some(join),
        }
    }

    pub fn load(&self, path: PathBuf) -> Result<LoadInfo> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(LlmCmd::Load {
                path,
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("LLM worker thread stopped"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("LLM worker dropped load response"))?
    }

    pub fn unload(&self) {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send(LlmCmd::Unload { reply: reply_tx }).is_ok() {
            let _ = reply_rx.recv();
        }
    }

    pub fn fix_text(&self, text: String) -> Result<(String, u128)> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(LlmCmd::FixText {
                text,
                reply: reply_tx,
            })
            .map_err(|_| anyhow!("LLM worker thread stopped"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("LLM worker dropped fix response"))?
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.send(LlmCmd::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for LlmHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(rx: Receiver<LlmCmd>) {
    let mut backend = match LlamaBackend::init() {
        Ok(b) => b,
        Err(err) => {
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    LlmCmd::Load { reply, .. } => {
                        let _ = reply.send(Err(anyhow!("Failed to init llama backend: {err}")));
                    }
                    LlmCmd::Unload { reply } => {
                        let _ = reply.send(());
                    }
                    LlmCmd::FixText { reply, .. } => {
                        let _ = reply.send(Err(anyhow!("Llama backend unavailable: {err}")));
                    }
                    LlmCmd::Shutdown => break,
                }
            }
            return;
        }
    };
    backend.void_logs();

    let mut pending_load: Option<(PathBuf, Sender<Result<LoadInfo>>)> = None;

    loop {
        let (path, reply) = if let Some(item) = pending_load.take() {
            item
        } else {
            match rx.recv() {
                Ok(LlmCmd::Load { path, reply }) => (path, reply),
                Ok(LlmCmd::Unload { reply }) => {
                    let _ = reply.send(());
                    continue;
                }
                Ok(LlmCmd::FixText { reply, .. }) => {
                    let _ = reply.send(Err(anyhow!("Model is not loaded")));
                    continue;
                }
                Ok(LlmCmd::Shutdown) | Err(_) => break,
            }
        };

        let (model, backend_desc) = match load_model_only(&backend, &path) {
            Ok(res) => res,
            Err(err) => {
                let _ = reply.send(Err(err));
                continue;
            }
        };

        let num_threads = std::thread::available_parallelism()
            .map(|n| n.get().min(8) as i32)
            .unwrap_or(4);

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(N_CTX))
            .with_n_batch(N_BATCH as u32)
            .with_n_threads(num_threads)
            .with_n_threads_batch(num_threads);

        let mut ctx = match model
            .new_context(&backend, ctx_params)
            .context("Failed to create llama inference context")
        {
            Ok(c) => c,
            Err(err) => {
                let _ = reply.send(Err(err));
                continue;
            }
        };

        let style = detect_prompt_style(&model, &path);

        if style == PromptStyle::QwenNoThink {
            let special_check = model.vocab().tokenize(b"<|im_start|>", false, true);
            if special_check.len() != 1 {
                drop(ctx);
                drop(model);
                let _ = reply.send(Err(anyhow!(
                    "Expected <|im_start|> to tokenize as 1 special token, got {}",
                    special_check.len()
                )));
                continue;
            }
        }

        if let Err(err) = warmup_context(&model, &mut ctx, style) {
            drop(ctx);
            drop(model);
            let _ = reply.send(Err(err));
            continue;
        }

        let model_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("model.gguf")
            .to_string();

        let _ = reply.send(Ok(LoadInfo {
            backend_desc,
            model_name,
        }));

        // Inner loop keeps `ctx` and `model` alive cleanly on stack until Unload/Reload/Shutdown.
        let mut should_exit = false;
        loop {
            match rx.recv() {
                Ok(LlmCmd::FixText { text, reply }) => {
                    let res = run_fix(&model, &mut ctx, style, &text);
                    let _ = reply.send(res);
                }
                Ok(LlmCmd::Unload { reply }) => {
                    drop(ctx);
                    drop(model);
                    let _ = reply.send(());
                    break;
                }
                Ok(LlmCmd::Load {
                    path: next_path,
                    reply: next_reply,
                }) => {
                    drop(ctx);
                    drop(model);
                    pending_load = Some((next_path, next_reply));
                    break;
                }
                Ok(LlmCmd::Shutdown) | Err(_) => {
                    drop(ctx);
                    drop(model);
                    should_exit = true;
                    break;
                }
            }
        }
        if should_exit {
            break;
        }
    }
}

fn load_model_only(backend: &LlamaBackend, path: &Path) -> Result<(LlamaModel, String)> {
    if !path.exists() {
        bail!("Model file does not exist: {}", path.display());
    }

    match LlamaModel::load_from_file(
        backend,
        path,
        &LlamaModelParams::default().with_n_gpu_layers(u32::MAX),
    ) {
        Ok(m) => Ok((m, "Vulkan GPU".to_string())),
        Err(_gpu_err) => {
            let m = LlamaModel::load_from_file(
                backend,
                path,
                &LlamaModelParams::default().with_n_gpu_layers(0),
            )
            .with_context(|| format!("Failed to load model from {}", path.display()))?;
            Ok((m, "CPU".to_string()))
        }
    }
}

fn detect_prompt_style(model: &LlamaModel, path: &Path) -> PromptStyle {
    let fname = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if let Ok(tmpl) = model.chat_template(None) {
        let tmpl_str = tmpl.to_str().unwrap_or("");
        if tmpl_str.contains("<|im_start|>")
            && (tmpl_str.contains("<think>") || fname.contains("qwen3"))
        {
            return PromptStyle::QwenNoThink;
        }
        return PromptStyle::EmbeddedTemplate;
    }

    if fname.contains("qwen3") {
        PromptStyle::QwenNoThink
    } else {
        PromptStyle::ChatMlFallback
    }
}

fn warmup_context(
    model: &LlamaModel,
    ctx: &mut LlamaContext<'_>,
    style: PromptStyle,
) -> Result<()> {
    ctx.clear_kv_cache();
    let vocab = model.vocab();
    let (probe, add_bos) = build_prompt(model, style, "helo wrold")?;
    let tokens = vocab.tokenize(probe.as_bytes(), add_bos, true);
    if !tokens.is_empty() {
        let mut batch = LlamaBatch::new(tokens.len().max(8), 1);
        let last = tokens.len() - 1;
        for (i, &tok) in tokens.iter().enumerate() {
            batch.add(tok, i as i32, &[0], i == last)?;
        }
        ctx.decode(&mut batch).context("Warmup prefill failed")?;

        let mut sampler = LlamaSampler::greedy();
        let mut n_cur = tokens.len() as i32;
        for _ in 0..3 {
            let token = sampler.sample(ctx, batch.n_tokens() - 1);
            if vocab.is_eog(token) {
                break;
            }
            batch.clear();
            batch.add(token, n_cur, &[0], true)?;
            n_cur += 1;
            ctx.decode(&mut batch).context("Warmup decode failed")?;
        }
    }
    ctx.clear_kv_cache();
    Ok(())
}

fn build_prompt(model: &LlamaModel, style: PromptStyle, user_text: &str) -> Result<(String, bool)> {
    match style {
        PromptStyle::QwenNoThink => {
            let prompt = format!(
                "<|im_start|>system\n{SYSTEM_PROMPT}<|im_end|>\n<|im_start|>user\n{user_text}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
            );
            Ok((prompt, false))
        }
        PromptStyle::EmbeddedTemplate => {
            let tmpl = model
                .chat_template(None)
                .map_err(|e| anyhow!("Missing chat template: {e}"))?;
            let msgs = vec![
                LlamaChatMessage::new("system".to_string(), SYSTEM_PROMPT.to_string())?,
                LlamaChatMessage::new("user".to_string(), user_text.to_string())?,
            ];
            let mut prompt = model
                .apply_chat_template(&tmpl, &msgs, true)
                .context("Failed to apply model chat template")?;
            if prompt.ends_with("<think>\n") {
                prompt.push_str("\n</think>\n\n");
            } else if prompt.ends_with("<think>") {
                prompt.push_str("\n\n</think>\n\n");
            }
            Ok((prompt, model.vocab().should_add_bos()))
        }
        PromptStyle::ChatMlFallback => {
            let fallback = LlamaChatTemplate::new("chatml")?;
            let msgs = vec![
                LlamaChatMessage::new("system".to_string(), SYSTEM_PROMPT.to_string())?,
                LlamaChatMessage::new("user".to_string(), user_text.to_string())?,
            ];
            let prompt = model
                .apply_chat_template(&fallback, &msgs, true)
                .context("Failed to apply ChatML fallback template")?;
            Ok((prompt, model.vocab().should_add_bos()))
        }
    }
}

fn split_outer_whitespace(text: &str) -> (&str, &str, &str) {
    let trimmed_start = text.trim_start();
    let leading_len = text.len() - trimmed_start.len();
    let core = trimmed_start.trim_end();
    let trailing_len = trimmed_start.len() - core.len();
    let leading = &text[..leading_len];
    let trailing = &trimmed_start[trimmed_start.len() - trailing_len..];
    (leading, core, trailing)
}

pub fn clean_output(raw: &str, original_core: &str) -> String {
    let mut s = if let Some(idx) = raw.rfind("</think>") {
        &raw[idx + "</think>".len()..]
    } else {
        raw
    };

    for stop in ["<|im_end|>", "<|endoftext|>", "<|eot_id|>"] {
        if let Some(idx) = s.find(stop) {
            s = &s[..idx];
        }
    }

    let mut out = s.trim().to_string();

    if !(original_core.starts_with("```") && original_core.ends_with("```")) {
        if out.starts_with("```") && out.ends_with("```") && out.len() >= 6 {
            let inner = &out[3..out.len() - 3];
            let inner = if let Some(nl) = inner.find('\n') {
                let first_line = inner[..nl].trim();
                if first_line.len() <= 20 && !first_line.contains(' ') {
                    &inner[nl + 1..]
                } else {
                    inner
                }
            } else {
                inner
            };
            out = inner.trim().to_string();
        }
    }

    if !(original_core.starts_with('"') && original_core.ends_with('"')) {
        if out.starts_with('"') && out.ends_with('"') && out.len() >= 2 {
            let inner = &out[1..out.len() - 1];
            if !inner.contains('"') || original_core.contains('"') {
                out = inner.trim().to_string();
            }
        }
    }

    out
}

fn infer_once(
    model: &LlamaModel,
    ctx: &mut LlamaContext<'_>,
    style: PromptStyle,
    core_lf: &str,
) -> Result<String> {
    if core_lf.trim().is_empty() {
        return Ok(core_lf.to_string());
    }

    let vocab = model.vocab();
    let (prompt, add_bos) = build_prompt(model, style, core_lf)?;
    let prompt_tokens = vocab.tokenize(prompt.as_bytes(), add_bos, true);

    if prompt_tokens.len() + 64 >= N_CTX as usize {
        bail!("Selected text is too long ({} tokens)", prompt_tokens.len());
    }

    let input_tokens_count = vocab.tokenize(core_lf.as_bytes(), false, false).len();
    let remaining_ctx = (N_CTX as usize).saturating_sub(prompt_tokens.len());
    let max_new_tokens = ((input_tokens_count as f32 * 1.45).ceil() as usize)
        .max(64)
        .min(remaining_ctx);

    ctx.clear_kv_cache();

    let mut batch = LlamaBatch::new(N_BATCH, 1);
    let total_prompt = prompt_tokens.len();
    for (chunk_idx, chunk) in prompt_tokens.chunks(N_BATCH).enumerate() {
        batch.clear();
        let base_pos = chunk_idx * N_BATCH;
        for (i, &tok) in chunk.iter().enumerate() {
            let pos = base_pos + i;
            let is_last = pos + 1 == total_prompt;
            batch.add(tok, pos as i32, &[0], is_last)?;
        }
        ctx.decode(&mut batch)
            .context("Failed to decode prompt batch")?;
    }

    let mut sampler = LlamaSampler::greedy();
    let mut n_cur = total_prompt as i32;
    let mut output_bytes: Vec<u8> = Vec::with_capacity(core_lf.len() + 32);

    for _ in 0..max_new_tokens {
        let token = sampler.sample(ctx, batch.n_tokens() - 1);
        if vocab.is_eog(token) {
            break;
        }

        vocab.token_to_piece_into(token, &mut output_bytes, false, None);

        let lossy = String::from_utf8_lossy(&output_bytes);
        if lossy.ends_with("<|im_end|>") || lossy.ends_with("<|endoftext|>") {
            break;
        }

        batch.clear();
        batch.add(token, n_cur, &[0], true)?;
        n_cur += 1;
        ctx.decode(&mut batch)
            .context("Failed to decode generated token")?;
    }

    let raw_output = String::from_utf8_lossy(&output_bytes);
    let cleaned_core = clean_output(&raw_output, core_lf);
    if cleaned_core.is_empty() {
        Ok(core_lf.to_string())
    } else {
        Ok(cleaned_core)
    }
}

fn run_fix(
    model: &LlamaModel,
    ctx: &mut LlamaContext<'_>,
    style: PromptStyle,
    original_text: &str,
) -> Result<(String, u128)> {
    let started = Instant::now();
    if original_text.trim().is_empty() {
        return Ok((original_text.to_string(), 0));
    }

    let had_crlf = original_text.contains("\r\n");
    let (leading_ws, core, trailing_ws) = split_outer_whitespace(original_text);
    let core_lf = core.replace("\r\n", "\n");

    let mut cleaned_core = infer_once(model, ctx, style, &core_lf)?;

    // Safety net: if the model collapsed explicit line breaks into a single line, fix line-by-line.
    if core_lf.contains('\n') && !cleaned_core.contains('\n') {
        let mut fixed_lines = Vec::new();
        for line in core_lf.split('\n') {
            let (l_ws, l_core, t_ws) = split_outer_whitespace(line);
            if l_core.is_empty() {
                fixed_lines.push(line.to_string());
            } else {
                let fixed = infer_once(model, ctx, style, l_core)?;
                fixed_lines.push(format!("{l_ws}{fixed}{t_ws}"));
            }
        }
        cleaned_core = fixed_lines.join("\n");
    }

    let restored_core = if had_crlf {
        cleaned_core.replace('\n', "\r\n")
    } else {
        cleaned_core
    };

    let final_text = format!("{leading_ws}{restored_core}{trailing_ws}");
    Ok((final_text, started.elapsed().as_millis()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_output_strips_think_and_preserves_multiline_and_profanity() {
        let raw = "<think>\nlet me fix this\n</think>\n\nThis damn boss fight is kicking my ass.\nWhat am I supposed to do?";
        let orig = "this damn boss fight is kicking my ass\nwhat am i suppose to do";
        assert_eq!(
            clean_output(raw, orig),
            "This damn boss fight is kicking my ass.\nWhat am I supposed to do?"
        );
    }

    #[test]
    fn clean_output_strips_accidental_quotes_and_fences_only_when_not_in_original() {
        assert_eq!(
            clean_output("\"Hello world.\"", "hello wrold"),
            "Hello world."
        );
        assert_eq!(
            clean_output("\"Hello world.\"", "\"hello wrold\""),
            "\"Hello world.\""
        );
        assert_eq!(
            clean_output("```text\nFixed line.\n```", "fixed lne"),
            "Fixed line."
        );
    }

    #[test]
    fn split_outer_whitespace_roundtrips_cleanly() {
        let (l, c, t) = split_outer_whitespace("  hello world \n");
        assert_eq!(l, "  ");
        assert_eq!(c, "hello world");
        assert_eq!(t, " \n");
    }
}
