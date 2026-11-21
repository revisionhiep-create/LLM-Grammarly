use anyhow::{anyhow, bail, Context, Result};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use llama_cpp_2::vocab::LlamaVocab;
use llama_cpp_2::{list_llama_ggml_backend_devices, LlamaBackendDeviceType};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::config::ComputeMode;

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
        compute_mode: ComputeMode,
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

struct PromptConfig {
    style: PromptStyle,
    template: Option<LlamaChatTemplate>,
    overhead_tokens: usize,
}

enum SessionExit {
    Unloaded(Sender<()>),
    Reload(PathBuf, ComputeMode, Sender<Result<LoadInfo>>),
    Shutdown,
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

    pub fn load(&self, path: PathBuf, compute_mode: ComputeMode) -> Result<LoadInfo> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(LlmCmd::Load {
                path,
                compute_mode,
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

fn has_vulkan_gpu_device() -> bool {
    list_llama_ggml_backend_devices().into_iter().any(|d| {
        matches!(
            d.device_type,
            LlamaBackendDeviceType::Gpu | LlamaBackendDeviceType::IntegratedGpu
        )
    })
}

fn compute_thread_counts_for_logical(logical: usize) -> (i32, i32) {
    let decode_threads = if logical >= 4 {
        (logical / 2).clamp(2, 8)
    } else {
        logical.max(1)
    } as i32;
    let batch_threads = logical.clamp(1, 8) as i32;
    (decode_threads, batch_threads)
}

fn compute_thread_counts() -> (i32, i32) {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    compute_thread_counts_for_logical(logical)
}

fn run_loaded_session(
    rx: &Receiver<LlmCmd>,
    model: &LlamaModel,
    ctx: &mut LlamaContext<'_>,
    batch: &mut LlamaBatch<'_>,
    n_ctx: usize,
    prompt_cfg: &PromptConfig,
) -> SessionExit {
    loop {
        match rx.recv() {
            Ok(LlmCmd::FixText { text, reply }) => {
                let res = run_fix(model, ctx, batch, n_ctx, prompt_cfg, &text);
                let _ = reply.send(res);
            }
            Ok(LlmCmd::Unload { reply }) => return SessionExit::Unloaded(reply),
            Ok(LlmCmd::Load {
                path,
                compute_mode,
                reply,
            }) => return SessionExit::Reload(path, compute_mode, reply),
            Ok(LlmCmd::Shutdown) | Err(_) => return SessionExit::Shutdown,
        }
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

    let (decode_threads, batch_threads) = compute_thread_counts();
    let mut pending_load: Option<(PathBuf, ComputeMode, Sender<Result<LoadInfo>>)> = None;

    loop {
        let (path, compute_mode, reply) = if let Some(item) = pending_load.take() {
            item
        } else {
            match rx.recv() {
                Ok(LlmCmd::Load {
                    path,
                    compute_mode,
                    reply,
                }) => (path, compute_mode, reply),
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

        if !path.exists() {
            let _ = reply.send(Err(anyhow!(
                "Model file does not exist: {}",
                path.display()
            )));
            continue;
        }

        let want_gpu = compute_mode == ComputeMode::Auto && has_vulkan_gpu_device();
        let attempts: &[bool] = if want_gpu { &[true, false] } else { &[false] };

        let mut reply = Some(reply);
        let mut last_err: Option<anyhow::Error> = None;
        let mut exit_action: Option<SessionExit> = None;

        for &use_gpu in attempts {
            let n_gpu_layers = if use_gpu { u32::MAX } else { 0 };
            let model = match LlamaModel::load_from_file(
                &backend,
                &path,
                &LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers),
            )
            .with_context(|| format!("Failed to load model from {}", path.display()))
            {
                Ok(m) => m,
                Err(err) => {
                    last_err = Some(err);
                    continue;
                }
            };

            let n_ctx = N_CTX.min(model.n_ctx_train().max(512));
            let ctx_params = LlamaContextParams::default()
                .with_n_ctx(NonZeroU32::new(n_ctx))
                .with_n_batch(N_BATCH as u32)
                .with_n_threads(decode_threads)
                .with_n_threads_batch(batch_threads);

            let mut ctx = match model
                .new_context(&backend, ctx_params)
                .context("Failed to create llama inference context")
            {
                Ok(c) => c,
                Err(err) => {
                    last_err = Some(err);
                    continue;
                }
            };

            let prompt_cfg = match build_prompt_config(&model, &path) {
                Ok(cfg) => cfg,
                Err(err) => {
                    last_err = Some(err);
                    continue;
                }
            };

            let mut batch = LlamaBatch::new(N_BATCH, 1);
            if let Err(err) = warmup_context(&model, &mut ctx, &mut batch, &prompt_cfg) {
                last_err = Some(err);
                continue;
            }

            let backend_desc = if use_gpu { "Vulkan GPU" } else { "CPU" }.to_string();
            let model_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("model.gguf")
                .to_string();

            if let Some(tx) = reply.take() {
                let _ = tx.send(Ok(LoadInfo {
                    backend_desc,
                    model_name,
                }));
            }

            exit_action = Some(run_loaded_session(
                &rx,
                &model,
                &mut ctx,
                &mut batch,
                n_ctx as usize,
                &prompt_cfg,
            ));
            break;
        }

        // `ctx` and `model` from the loop have already been dropped here, releasing VRAM/RAM before signaling Unload/Reload.
        match exit_action {
            None => {
                if let Some(tx) = reply.take() {
                    let _ = tx.send(Err(
                        last_err.unwrap_or_else(|| anyhow!("Failed to load model"))
                    ));
                }
            }
            Some(SessionExit::Unloaded(unload_reply)) => {
                let _ = unload_reply.send(());
            }
            Some(SessionExit::Reload(next_path, next_mode, next_reply)) => {
                pending_load = Some((next_path, next_mode, next_reply));
            }
            Some(SessionExit::Shutdown) => break,
        }
    }
}

fn detect_prompt_style(model: &LlamaModel, path: &Path) -> PromptStyle {
    let fname = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let has_im_start = model.vocab().tokenize(b"<|im_start|>", false, true).len() == 1;

    if let Ok(tmpl) = model.chat_template(None) {
        let tmpl_str = tmpl.to_str().unwrap_or("");
        if has_im_start
            && tmpl_str.contains("<|im_start|>")
            && (tmpl_str.contains("<think>") || fname.contains("qwen3"))
        {
            return PromptStyle::QwenNoThink;
        }
        return PromptStyle::EmbeddedTemplate;
    }

    if has_im_start && fname.contains("qwen3") {
        PromptStyle::QwenNoThink
    } else {
        PromptStyle::ChatMlFallback
    }
}

fn should_prepend_bos(vocab: &LlamaVocab<'_>, prompt: &str) -> bool {
    if !vocab.should_add_bos() {
        return false;
    }
    let bos = vocab.bos();
    if bos.0 >= 0 {
        if let Some(cstr) = vocab.text(bos) {
            if let Ok(bos_text) = cstr.to_str() {
                if !bos_text.is_empty() && prompt.starts_with(bos_text) {
                    return false;
                }
            }
        }
    }
    true
}

fn build_prompt_config(model: &LlamaModel, path: &Path) -> Result<PromptConfig> {
    let style = detect_prompt_style(model, path);
    let template = match style {
        PromptStyle::QwenNoThink => None,
        PromptStyle::EmbeddedTemplate => Some(
            model
                .chat_template(None)
                .map_err(|e| anyhow!("Missing chat template: {e}"))?,
        ),
        PromptStyle::ChatMlFallback => Some(LlamaChatTemplate::new("chatml")?),
    };

    let mut cfg = PromptConfig {
        style,
        template,
        overhead_tokens: 0,
    };
    cfg.overhead_tokens = tokenize_prompt(model, &cfg, "")?.len();
    Ok(cfg)
}

fn make_unique_sentinel(user_text: &str) -> String {
    let mut counter = 0usize;
    loop {
        let candidate = format!("__KBLLM_USER_TEXT_SENTINEL_{counter}__");
        if !user_text.contains(&candidate) {
            return candidate;
        }
        counter += 1;
    }
}

fn tokenize_prompt(
    model: &LlamaModel,
    cfg: &PromptConfig,
    user_text: &str,
) -> Result<Vec<LlamaToken>> {
    let vocab = model.vocab();
    match cfg.style {
        PromptStyle::QwenNoThink => {
            let prefix = format!(
                "<|im_start|>system\n{SYSTEM_PROMPT}<|im_end|>\n<|im_start|>user\n"
            );
            let suffix = "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";
            let mut tokens = vocab.tokenize(prefix.as_bytes(), false, true);
            if !user_text.is_empty() {
                tokens.extend(vocab.tokenize(user_text.as_bytes(), false, false));
            }
            tokens.extend(vocab.tokenize(suffix.as_bytes(), false, true));
            Ok(tokens)
        }
        PromptStyle::EmbeddedTemplate | PromptStyle::ChatMlFallback => {
            let tmpl = cfg.template.as_ref().ok_or_else(|| {
                let kind = match cfg.style {
                    PromptStyle::ChatMlFallback => "ChatML fallback",
                    _ => "chat",
                };
                anyhow!("Missing cached {kind} template")
            })?;
            let sentinel = make_unique_sentinel(user_text);
            let msgs = vec![
                LlamaChatMessage::new("system".to_string(), SYSTEM_PROMPT.to_string())?,
                LlamaChatMessage::new("user".to_string(), sentinel.clone())?,
            ];
            let mut rendered = model
                .apply_chat_template(tmpl, &msgs, true)
                .context(match cfg.style {
                    PromptStyle::ChatMlFallback => "Failed to apply ChatML fallback template",
                    _ => "Failed to apply model chat template",
                })?;
            if cfg.style == PromptStyle::EmbeddedTemplate {
                if rendered.ends_with("<think>\n") {
                    rendered.push_str("\n</think>\n\n");
                } else if rendered.ends_with("<think>") {
                    rendered.push_str("\n\n</think>\n\n");
                }
            }
            let add_bos = should_prepend_bos(&vocab, &rendered);
            if let Some((prefix, suffix)) = rendered.split_once(&sentinel) {
                let mut tokens = vocab.tokenize(prefix.as_bytes(), add_bos, true);
                if !user_text.is_empty() {
                    tokens.extend(vocab.tokenize(user_text.as_bytes(), false, false));
                }
                tokens.extend(vocab.tokenize(suffix.as_bytes(), false, true));
                Ok(tokens)
            } else {
                let (prompt, add_bos) = build_prompt(model, cfg, user_text)?;
                Ok(vocab.tokenize(prompt.as_bytes(), add_bos, true))
            }
        }
    }
}

fn decode_prompt_tokens(
    ctx: &mut LlamaContext<'_>,
    batch: &mut LlamaBatch<'_>,
    tokens: &[LlamaToken],
) -> Result<()> {
    let total_prompt = tokens.len();
    for (chunk_idx, chunk) in tokens.chunks(N_BATCH).enumerate() {
        batch.clear();
        let base_pos = chunk_idx * N_BATCH;
        for (i, &tok) in chunk.iter().enumerate() {
            let pos = base_pos + i;
            let is_last = pos + 1 == total_prompt;
            batch.add(tok, pos as i32, &[0], is_last)?;
        }
        ctx.decode(batch)
            .context("Failed to decode prompt batch")?;
    }
    Ok(())
}

fn warmup_context(
    model: &LlamaModel,
    ctx: &mut LlamaContext<'_>,
    batch: &mut LlamaBatch<'_>,
    prompt_cfg: &PromptConfig,
) -> Result<()> {
    ctx.clear_kv_cache();
    let vocab = model.vocab();
    let tokens = tokenize_prompt(model, prompt_cfg, "helo wrold")?;
    if !tokens.is_empty() {
        decode_prompt_tokens(ctx, batch, &tokens)?;

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
            ctx.decode(batch).context("Warmup decode failed")?;
        }
    }
    ctx.clear_kv_cache();
    Ok(())
}

fn build_prompt(
    model: &LlamaModel,
    cfg: &PromptConfig,
    user_text: &str,
) -> Result<(String, bool)> {
    match cfg.style {
        PromptStyle::QwenNoThink => {
            let prompt = format!(
                "<|im_start|>system\n{SYSTEM_PROMPT}<|im_end|>\n<|im_start|>user\n{user_text}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
            );
            Ok((prompt, false))
        }
        PromptStyle::EmbeddedTemplate => {
            let tmpl = cfg
                .template
                .as_ref()
                .ok_or_else(|| anyhow!("Missing cached chat template"))?;
            let msgs = vec![
                LlamaChatMessage::new("system".to_string(), SYSTEM_PROMPT.to_string())?,
                LlamaChatMessage::new("user".to_string(), user_text.to_string())?,
            ];
            let mut prompt = model
                .apply_chat_template(tmpl, &msgs, true)
                .context("Failed to apply model chat template")?;
            if prompt.ends_with("<think>\n") {
                prompt.push_str("\n</think>\n\n");
            } else if prompt.ends_with("<think>") {
                prompt.push_str("\n\n</think>\n\n");
            }
            let add_bos = should_prepend_bos(&model.vocab(), &prompt);
            Ok((prompt, add_bos))
        }
        PromptStyle::ChatMlFallback => {
            let fallback = cfg
                .template
                .as_ref()
                .ok_or_else(|| anyhow!("Missing cached ChatML fallback template"))?;
            let msgs = vec![
                LlamaChatMessage::new("system".to_string(), SYSTEM_PROMPT.to_string())?,
                LlamaChatMessage::new("user".to_string(), user_text.to_string())?,
            ];
            let prompt = model
                .apply_chat_template(fallback, &msgs, true)
                .context("Failed to apply ChatML fallback template")?;
            let add_bos = should_prepend_bos(&model.vocab(), &prompt);
            Ok((prompt, add_bos))
        }
    }
}

fn ends_with_stop_sequence(bytes: &[u8]) -> bool {
    bytes.ends_with(b"<|im_end|>")
        || bytes.ends_with(b"<|endoftext|>")
        || bytes.ends_with(b"<|eot_id|>")
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
    let trimmed_raw = raw.trim_start();
    let mut s = if trimmed_raw.starts_with("<think>") {
        if let Some(idx) = trimmed_raw.find("</think>") {
            &trimmed_raw[idx + "</think>".len()..]
        } else {
            raw
        }
    } else if !original_core.contains("</think>") {
        if let Some(idx) = raw.rfind("</think>") {
            &raw[idx + "</think>".len()..]
        } else {
            raw
        }
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

    if !original_core.starts_with('"') && !original_core.ends_with('"') {
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
    batch: &mut LlamaBatch<'_>,
    n_ctx: usize,
    prompt_cfg: &PromptConfig,
    core_lf: &str,
) -> Result<String> {
    if core_lf.trim().is_empty() {
        return Ok(core_lf.to_string());
    }

    let vocab = model.vocab();
    let prompt_tokens = tokenize_prompt(model, prompt_cfg, core_lf)?;

    if prompt_tokens.len() + 64 >= n_ctx {
        bail!("Selected text is too long ({} tokens)", prompt_tokens.len());
    }

    let input_tokens_count = prompt_tokens
        .len()
        .saturating_sub(prompt_cfg.overhead_tokens)
        .max(1);
    let remaining_ctx = n_ctx.saturating_sub(prompt_tokens.len());
    let max_new_tokens = ((input_tokens_count as f32 * 1.45).ceil() as usize)
        .max(64)
        .min(remaining_ctx);

    ctx.clear_kv_cache();
    decode_prompt_tokens(ctx, batch, &prompt_tokens)?;

    let mut sampler = LlamaSampler::greedy();
    let mut n_cur = prompt_tokens.len() as i32;
    let mut output_bytes: Vec<u8> = Vec::with_capacity(core_lf.len() + 32);

    for _ in 0..max_new_tokens {
        let token = sampler.sample(ctx, batch.n_tokens() - 1);
        if vocab.is_eog(token) {
            break;
        }

        vocab.token_to_piece_into(token, &mut output_bytes, true, None);

        if ends_with_stop_sequence(&output_bytes) {
            break;
        }

        batch.clear();
        batch.add(token, n_cur, &[0], true)?;
        n_cur += 1;
        ctx.decode(batch)
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
    batch: &mut LlamaBatch<'_>,
    n_ctx: usize,
    prompt_cfg: &PromptConfig,
    original_text: &str,
) -> Result<(String, u128)> {
    let started = Instant::now();
    if original_text.trim().is_empty() {
        return Ok((original_text.to_string(), 0));
    }

    let had_crlf = original_text.contains("\r\n");
    let (leading_ws, core, trailing_ws) = split_outer_whitespace(original_text);
    let core_lf = core.replace("\r\n", "\n");

    let mut cleaned_core = infer_once(model, ctx, batch, n_ctx, prompt_cfg, &core_lf)?;

    // Safety net: if the model collapsed explicit line breaks into a single line, fix line-by-line.
    if core_lf.contains('\n') && !cleaned_core.contains('\n') {
        let mut fixed_lines = Vec::new();
        for line in core_lf.split('\n') {
            let (l_ws, l_core, t_ws) = split_outer_whitespace(line);
            if l_core.is_empty() {
                fixed_lines.push(line.to_string());
            } else {
                let fixed = infer_once(model, ctx, batch, n_ctx, prompt_cfg, l_core)?;
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
    fn clean_output_strips_eot_id_and_detects_stop_sequences() {
        assert_eq!(
            clean_output("Fixed text.<|eot_id|>trailing", "fxed txt"),
            "Fixed text."
        );
        assert!(ends_with_stop_sequence(b"hello<|im_end|>"));
        assert!(ends_with_stop_sequence(b"hello<|endoftext|>"));
        assert!(ends_with_stop_sequence(b"hello<|eot_id|>"));
        assert!(!ends_with_stop_sequence(b"hello world"));
    }

    #[test]
    fn compute_thread_counts_avoids_hyperthread_thrashing_on_decode() {
        assert_eq!(compute_thread_counts_for_logical(2), (2, 2));
        assert_eq!(compute_thread_counts_for_logical(4), (2, 4));
        assert_eq!(compute_thread_counts_for_logical(8), (4, 8));
        assert_eq!(compute_thread_counts_for_logical(16), (8, 8));
        assert_eq!(compute_thread_counts_for_logical(32), (8, 8));
    }

    #[test]
    fn split_outer_whitespace_roundtrips_cleanly() {
        let (l, c, t) = split_outer_whitespace("  hello world \n");
        assert_eq!(l, "  ");
        assert_eq!(c, "hello world");
        assert_eq!(t, " \n");
    }

    #[test]
    fn clean_output_preserves_literal_closing_think_and_missing_closing_quote() {
        assert_eq!(
            clean_output("Explain the </think> tag.", "explain </think> tag"),
            "Explain the </think> tag."
        );
        assert_eq!(
            clean_output(
                "<think>checking</think>\nExplain the </think> tag.",
                "explain </think> tag"
            ),
            "Explain the </think> tag."
        );
        assert_eq!(
            clean_output("\"Hello,\" he said, \"bye.\"", "\"hello\" he said \"bye"),
            "\"Hello,\" he said, \"bye.\""
        );
    }

    #[test]
    fn make_unique_sentinel_avoids_user_text_collisions() {
        let base = make_unique_sentinel("plain text");
        assert_eq!(base, "__KBLLM_USER_TEXT_SENTINEL_0__");
        let avoided = make_unique_sentinel("has __KBLLM_USER_TEXT_SENTINEL_0__ inside");
        assert_eq!(avoided, "__KBLLM_USER_TEXT_SENTINEL_1__");
        assert!(!avoided.eq(&base));
    }
}
