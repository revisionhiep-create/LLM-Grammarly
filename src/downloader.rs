use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{HeaderValue, CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
    pub speed_bps: f64,
}

fn verify_part_magic_if_exists(part_path: &Path) -> Result<u64> {
    if !part_path.exists() {
        return Ok(0);
    }
    let meta = fs::metadata(part_path)?;
    let len = meta.len();
    if len < 4 {
        let _ = fs::remove_file(part_path);
        return Ok(0);
    }
    let mut f = File::open(part_path)?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    if &magic != b"GGUF" {
        let _ = fs::remove_file(part_path);
        return Ok(0);
    }
    Ok(len)
}

fn parse_content_range(header: Option<&HeaderValue>) -> Option<(u64, Option<u64>)> {
    let text = header?.to_str().ok()?.trim();
    let rest = text
        .strip_prefix("bytes ")
        .or_else(|| text.strip_prefix("BYTES "))
        .or_else(|| {
            let mut split = text.splitn(2, ' ');
            let unit = split.next()?;
            unit.eq_ignore_ascii_case("bytes").then(|| split.next()).flatten()
        })?
        .trim();
    let (range_part, total_part) = rest.split_once('/')?;
    let (start_str, end_str) = range_part.trim().split_once('-')?;
    let start = start_str.trim().parse::<u64>().ok()?;
    let end = end_str.trim().parse::<u64>().ok()?;
    if end < start {
        return None;
    }
    let total = match total_part.trim() {
        "*" => None,
        s => Some(s.parse::<u64>().ok()?),
    };
    Some((start, total))
}

pub fn download_model(
    url: &str,
    dest: &Path,
    cancel: Arc<AtomicBool>,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create models folder {}", parent.display()))?;
    }

    let part_path = crate::config::partial_download_path(dest);
    let existing_len = verify_part_magic_if_exists(&part_path)?;

    let client = Client::builder()
        .user_agent("keyboard-llm/1.0")
        .connect_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
        .tcp_keepalive_interval(Duration::from_secs(10))
        .tcp_keepalive_retries(3)
        .timeout(None)
        .build()
        .context("Failed to build HTTP client")?;

    if cancel.load(Ordering::Relaxed) {
        bail!("Download cancelled");
    }

    let mut req = client.get(url);
    if existing_len > 0 {
        req = req.header(RANGE, format!("bytes={existing_len}-"));
    }

    let mut resp = req
        .send()
        .with_context(|| format!("Failed to connect to {url}"))?;

    if cancel.load(Ordering::Relaxed) {
        bail!("Download cancelled");
    }

    if existing_len > 0 && resp.status() == StatusCode::RANGE_NOT_SATISFIABLE {
        let _ = fs::remove_file(&part_path);
        resp = client
            .get(url)
            .send()
            .with_context(|| format!("Failed to reconnect to {url}"))?;
        if cancel.load(Ordering::Relaxed) {
            bail!("Download cancelled");
        }
    }

    if !resp.status().is_success() {
        bail!("Download failed (HTTP {})", resp.status());
    }

    let mut resume_range_total = None;
    let should_resume = if resp.status() == StatusCode::PARTIAL_CONTENT && existing_len > 0 {
        match parse_content_range(resp.headers().get(CONTENT_RANGE)) {
            Some((start, total_opt)) if start == existing_len => {
                resume_range_total = total_opt;
                true
            }
            _ => {
                drop(resp);
                let _ = fs::remove_file(&part_path);
                if cancel.load(Ordering::Relaxed) {
                    bail!("Download cancelled");
                }
                resp = client
                    .get(url)
                    .send()
                    .with_context(|| format!("Failed to reconnect to {url}"))?;
                if cancel.load(Ordering::Relaxed) {
                    bail!("Download cancelled");
                }
                if !resp.status().is_success() {
                    bail!("Download failed (HTTP {})", resp.status());
                }
                false
            }
        }
    } else {
        false
    };

    let (mut file, mut downloaded, total, mut checked_magic) = if should_resume {
        let total =
            resume_range_total.or_else(|| resp.content_length().map(|rem| existing_len + rem));
        let mut f = OpenOptions::new()
            .write(true)
            .open(&part_path)
            .with_context(|| format!("Failed to open {}", part_path.display()))?;
        f.seek(SeekFrom::End(0))?;
        (f, existing_len, total, true)
    } else {
        let total = resp.content_length();
        let f = File::create(&part_path)
            .with_context(|| format!("Failed to create {}", part_path.display()))?;
        (f, 0u64, total, false)
    };

    let mut header_buf = [0u8; 4];
    let mut header_fill = 0usize;
    let mut buf = vec![0u8; 256 * 1024];

    let mut last_emit = Instant::now();
    let mut window_start = Instant::now();
    let mut window_bytes = 0u64;
    let mut current_speed = 0.0f64;

    on_progress(DownloadProgress {
        downloaded,
        total,
        speed_bps: 0.0,
    });

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = file.flush();
            bail!("Download cancelled");
        }

        let n = resp
            .read(&mut buf)
            .context("Network error while downloading model")?;
        if n == 0 {
            break;
        }

        if !checked_magic {
            let take = (4 - header_fill).min(n);
            header_buf[header_fill..header_fill + take].copy_from_slice(&buf[..take]);
            header_fill += take;
            if header_fill == 4 {
                if &header_buf != b"GGUF" {
                    drop(file);
                    let _ = fs::remove_file(&part_path);
                    bail!("URL did not return a valid GGUF model file (check the Hugging Face link)");
                }
                checked_magic = true;
            }
        }

        file.write_all(&buf[..n])
            .context("Failed writing model bytes to disk")?;

        downloaded += n as u64;
        window_bytes += n as u64;

        let win_elapsed = window_start.elapsed().as_secs_f64();
        if win_elapsed >= 0.4 {
            current_speed = (window_bytes as f64) / win_elapsed;
            window_start = Instant::now();
            window_bytes = 0;
        }

        if last_emit.elapsed() >= Duration::from_millis(100) {
            on_progress(DownloadProgress {
                downloaded,
                total,
                speed_bps: current_speed,
            });
            last_emit = Instant::now();
        }
    }

    if !checked_magic {
        drop(file);
        let _ = fs::remove_file(&part_path);
        bail!("Downloaded file was too small to be a valid GGUF model");
    }

    if let Some(expected) = total {
        if downloaded < expected {
            bail!("Download ended early ({downloaded} of {expected} bytes received)");
        }
        if downloaded > expected {
            drop(file);
            let _ = fs::remove_file(&part_path);
            bail!("Downloaded size mismatch ({downloaded} bytes received, expected {expected})");
        }
    }

    file.flush()?;
    file.sync_all()?;
    drop(file);

    on_progress(DownloadProgress {
        downloaded,
        total: total.or(Some(downloaded)),
        speed_bps: current_speed,
    });

    if dest.exists() {
        let _ = fs::remove_file(dest);
    }
    fs::rename(&part_path, dest).with_context(|| {
        format!(
            "Failed to finalize model file {} -> {}",
            part_path.display(),
            dest.display()
        )
    })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range_headers() {
        let valid = HeaderValue::from_static("bytes 1024-2047/4096");
        assert_eq!(parse_content_range(Some(&valid)), Some((1024, Some(4096))));

        let unknown_total = HeaderValue::from_static("bytes 512-1023/*");
        assert_eq!(
            parse_content_range(Some(&unknown_total)),
            Some((512, None))
        );

        let unsatisfied = HeaderValue::from_static("bytes */4096");
        assert_eq!(parse_content_range(Some(&unsatisfied)), None);

        let bad_order = HeaderValue::from_static("bytes 200-100/500");
        assert_eq!(parse_content_range(Some(&bad_order)), None);

        let wrong_unit = HeaderValue::from_static("items 0-10/20");
        assert_eq!(parse_content_range(Some(&wrong_unit)), None);

        assert_eq!(parse_content_range(None), None);
    }
}
