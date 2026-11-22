use anyhow::{anyhow, bail, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::{default_backup_dir, fallback_backup_dir};

static PROBE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct ResolvedBackupDir {
    pub path: PathBuf,
    pub used_fallback: bool,
    pub is_custom: bool,
}

pub fn probe_dir_writable(dir: &Path) -> bool {
    if fs::create_dir_all(dir).is_err() {
        return false;
    }
    let seq = PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
    let probe = dir.join(format!(".kbllm_write_probe_{}_{seq}", std::process::id()));
    let ok = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .and_then(|mut f| f.write_all(b"ok"))
        .is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

pub fn resolve_backup_dir(configured: Option<&str>) -> ResolvedBackupDir {
    if let Some(custom) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        let p = PathBuf::from(custom);
        if probe_dir_writable(&p) {
            return ResolvedBackupDir {
                path: p,
                used_fallback: false,
                is_custom: true,
            };
        }
        let fb = fallback_backup_dir();
        let _ = fs::create_dir_all(&fb);
        return ResolvedBackupDir {
            path: fb,
            used_fallback: true,
            is_custom: false,
        };
    }

    let def = default_backup_dir();
    if probe_dir_writable(&def) {
        return ResolvedBackupDir {
            path: def,
            used_fallback: false,
            is_custom: false,
        };
    }

    let fb = fallback_backup_dir();
    let _ = fs::create_dir_all(&fb);
    ResolvedBackupDir {
        path: fb,
        used_fallback: true,
        is_custom: false,
    }
}

pub fn check_file_not_locked(path: &Path) -> Result<()> {
    let is_docx = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("docx"));
    if is_docx {
        if let (Some(parent), Some(fname)) = (
            path.parent(),
            path.file_name().and_then(|n| n.to_str()),
        ) {
            let lock_file = parent.join(format!("~${fname}"));
            if lock_file.exists() {
                bail!(
                    "File appears to be open in Microsoft Word (lock file {} found). Close it and try again, or use Save As.",
                    lock_file.display()
                );
            }
            // Microsoft Word for Mac creates `~$` replacing the first 2 characters when the name is >= 8 chars.
            if fname.chars().count() >= 8 {
                let truncated: String = fname.chars().skip(2).collect();
                let alt_lock = parent.join(format!("~${truncated}"));
                if alt_lock.exists() && fs::metadata(&alt_lock).is_ok_and(|m| m.len() < 512) {
                    bail!(
                        "File appears to be open in Microsoft Word (lock file {} found). Close it and try again, or use Save As.",
                        alt_lock.display()
                    );
                }
            }
        }
    }

    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            bail!(
                "Cannot write to {} (read-only or permission denied): {e}",
                path.display()
            )
        }
        Err(e) => bail!("Cannot access {}: {e}", path.display()),
    };

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(c) if c == libc::EWOULDBLOCK || c == libc::EAGAIN) {
            bail!(
                "File is currently open or locked by another application (e.g., Microsoft Word). Close it and try again, or use Save As."
            );
        }
    }
    Ok(())
}

pub fn local_timestamp_string() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm = std::mem::zeroed::<libc::tm>();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return "1970-01-01_000000".to_string();
        }
        format!(
            "{:04}-{:02}-{:02}_{:02}{:02}{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

fn is_valid_timestamp_part(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 17
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
        && b[10] == b'_'
        && b[11..17].iter().all(u8::is_ascii_digit)
}

pub fn parse_backup_sort_key(file_name: &str) -> Option<(String, u32)> {
    let base = match file_name.rsplit_once('.') {
        Some((stem, ext)) if !ext.is_empty() => stem,
        Some(_) => return None,
        None => file_name,
    };

    if base.len() > 18 {
        let split_at = base.len() - 18;
        if base.is_char_boundary(split_at) {
            let prefix = &base[..split_at];
            let sep_and_ts = &base[split_at..];
            if !prefix.is_empty()
                && sep_and_ts.starts_with('_')
                && is_valid_timestamp_part(&sep_and_ts[1..])
            {
                return Some((sep_and_ts[1..].to_string(), 0));
            }
        }
    }

    if let Some((before_seq, seq_str)) = base.rsplit_once('_') {
        if !seq_str.is_empty() && seq_str.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(seq) = seq_str.parse::<u32>() {
                if before_seq.len() > 18 {
                    let split_at = before_seq.len() - 18;
                    if before_seq.is_char_boundary(split_at) {
                        let prefix = &before_seq[..split_at];
                        let sep_and_ts = &before_seq[split_at..];
                        if !prefix.is_empty()
                            && sep_and_ts.starts_with('_')
                            && is_valid_timestamp_part(&sep_and_ts[1..])
                        {
                            return Some((sep_and_ts[1..].to_string(), seq));
                        }
                    }
                }
            }
        }
    }

    None
}

fn next_backup_path(backup_dir: &Path, src: &Path, stamp: &str) -> PathBuf {
    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("document");
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty());

    let format_name = |seq: Option<u32>| -> String {
        let base = match seq {
            None => format!("{stem}_{stamp}"),
            Some(n) => format!("{stem}_{stamp}_{n}"),
        };
        match ext {
            Some(e) => format!("{base}.{e}"),
            None => base,
        }
    };

    let first = backup_dir.join(format_name(None));
    if !first.exists() {
        return first;
    }
    let mut seq = 1u32;
    loop {
        let cand = backup_dir.join(format_name(Some(seq)));
        if !cand.exists() {
            return cand;
        }
        seq = seq.saturating_add(1);
    }
}

pub fn create_backup(src: &Path, backup_dir: &Path) -> Result<PathBuf> {
    create_backup_with_stamp(src, backup_dir, &local_timestamp_string())
}

fn create_backup_with_stamp(src: &Path, backup_dir: &Path, stamp: &str) -> Result<PathBuf> {
    fs::create_dir_all(backup_dir)
        .with_context(|| format!("Failed to create backup directory {}", backup_dir.display()))?;
    let src_len = fs::metadata(src)
        .with_context(|| format!("Failed to inspect source file {}", src.display()))?
        .len();

    let dest = next_backup_path(backup_dir, src, stamp);
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dest)
        .with_context(|| format!("Failed to create backup file {}", dest.display()))?;
    let mut inp = File::open(src)
        .with_context(|| format!("Failed to open source file {} for backup", src.display()))?;
    let copied = std::io::copy(&mut inp, &mut out)
        .with_context(|| format!("Failed to copy {} to backup {}", src.display(), dest.display()))?;
    out.flush()?;
    out.sync_all()?;
    drop(out);

    let dest_len = fs::metadata(&dest)?.len();
    if copied != src_len || dest_len != src_len {
        let _ = fs::remove_file(&dest);
        bail!(
            "Backup verification failed for {} (expected {src_len} bytes, wrote {dest_len})",
            dest.display()
        );
    }

    Ok(dest)
}

pub fn prune_backups(backup_dir: &Path, keep_last: usize) -> usize {
    let keep = keep_last.max(1);
    let Ok(entries) = fs::read_dir(backup_dir) else {
        return 0;
    };

    let mut managed: Vec<((String, u32, String), PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some((stamp, seq)) = parse_backup_sort_key(name) {
            managed.push(((stamp, seq, name.to_string()), path));
        }
    }

    if managed.len() <= keep {
        return 0;
    }

    managed.sort_by(|a, b| a.0.cmp(&b.0));
    let to_delete = managed.len() - keep;
    let mut deleted = 0usize;
    for (_, path) in managed.into_iter().take(to_delete) {
        if fs::remove_file(path).is_ok() {
            deleted += 1;
        }
    }
    deleted
}

fn replace_file_atomic(target: &Path, tmp: &Path) -> Result<()> {
    if let Ok(meta) = fs::metadata(target) {
        let _ = fs::set_permissions(tmp, meta.permissions());
    }
    fs::rename(tmp, target)
        .with_context(|| format!("Failed to save {}", target.display()))?;
    Ok(())
}

fn make_tmp_path(target: &Path) -> Result<PathBuf> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create destination folder {}", parent.display()))?;
    let name = target
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Invalid target filename {}", target.display()))?;
    Ok(parent.join(format!(".{name}.kbllm.tmp")))
}

pub fn atomic_save_with_backup<F, V>(
    target: &Path,
    backup_dir: &Path,
    keep_last: usize,
    write_tmp: F,
    validate_tmp: V,
) -> Result<PathBuf>
where
    F: FnOnce(&Path) -> Result<()>,
    V: FnOnce(&Path) -> Result<()>,
{
    if !target.exists() {
        bail!("Target file does not exist: {}", target.display());
    }
    check_file_not_locked(target)?;

    let backup_path = create_backup(target, backup_dir)?;
    let tmp_path = make_tmp_path(target)?;
    let _ = fs::remove_file(&tmp_path);

    if let Err(err) = write_tmp(&tmp_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    let meta_len = match fs::metadata(&tmp_path) {
        Ok(m) => m.len(),
        Err(err) => {
            let _ = fs::remove_file(&tmp_path);
            return Err(err.into());
        }
    };
    if meta_len == 0 {
        let _ = fs::remove_file(&tmp_path);
        bail!(
            "Refusing to overwrite {} with an empty output file",
            target.display()
        );
    }

    if let Err(err) = validate_tmp(&tmp_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = replace_file_atomic(target, &tmp_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    prune_backups(backup_dir, keep_last);
    Ok(backup_path)
}

pub fn atomic_save_new_file<F, V>(target: &Path, write_tmp: F, validate_tmp: V) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
    V: FnOnce(&Path) -> Result<()>,
{
    if target.exists() {
        check_file_not_locked(target)?;
    }

    let tmp_path = make_tmp_path(target)?;
    let _ = fs::remove_file(&tmp_path);

    if let Err(err) = write_tmp(&tmp_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    let meta_len = match fs::metadata(&tmp_path) {
        Ok(m) => m.len(),
        Err(err) => {
            let _ = fs::remove_file(&tmp_path);
            return Err(err.into());
        }
    };
    if meta_len == 0 {
        let _ = fs::remove_file(&tmp_path);
        bail!("Refusing to save an empty output file");
    }

    if let Err(err) = validate_tmp(&tmp_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    if let Err(err) = replace_file_atomic(target, &tmp_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let seq = PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "kbllm_backup_test_{name}_{}_{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_backup_sort_key_only_matches_managed_timestamps() {
        assert_eq!(
            parse_backup_sort_key("Report_2026-11-21_143022.docx"),
            Some(("2026-11-21_143022".to_string(), 0))
        );
        assert_eq!(
            parse_backup_sort_key("my_doc_v2_2026-11-21_143022_3.md"),
            Some(("2026-11-21_143022".to_string(), 3))
        );
        assert_eq!(parse_backup_sort_key("unrelated.txt"), None);
        assert_eq!(parse_backup_sort_key("notes_2026-11-21.txt"), None);
        assert_eq!(parse_backup_sort_key("2026-11-21_143022.txt"), None);
        assert_eq!(parse_backup_sort_key("Résumé — draft_notes_v1.docx"), None);
        assert_eq!(parse_backup_sort_key("日本語ファイル名_longer_2.txt"), None);
        assert_eq!(
            parse_backup_sort_key("Résumé_2026-11-21_143022.docx"),
            Some(("2026-11-21_143022".to_string(), 0))
        );
    }

    #[test]
    fn backup_collision_and_pruning_preserve_unrelated_files() {
        let root = temp_dir("prune");
        let backup_dir = root.join("Backups");
        fs::create_dir_all(&backup_dir).unwrap();

        let src = root.join("Report.txt");
        fs::write(&src, b"original body").unwrap();
        let unrelated = backup_dir.join("unrelated.txt");
        fs::write(&unrelated, b"keep me").unwrap();

        let b1 = create_backup_with_stamp(&src, &backup_dir, "2026-11-21_100000").unwrap();
        let b2 = create_backup_with_stamp(&src, &backup_dir, "2026-11-21_100000").unwrap();
        let b3 = create_backup_with_stamp(&src, &backup_dir, "2026-11-21_100005").unwrap();

        assert_eq!(b1.file_name().unwrap(), "Report_2026-11-21_100000.txt");
        assert_eq!(b2.file_name().unwrap(), "Report_2026-11-21_100000_1.txt");
        assert_eq!(b3.file_name().unwrap(), "Report_2026-11-21_100005.txt");

        let deleted = prune_backups(&backup_dir, 2);
        assert_eq!(deleted, 1);
        assert!(!b1.exists());
        assert!(b2.exists());
        assert!(b3.exists());
        assert!(unrelated.exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn atomic_save_aborts_on_validation_failure_without_touching_target() {
        let root = temp_dir("validate_fail");
        let backup_dir = root.join("Backups");
        let target = root.join("notes.txt");
        fs::write(&target, b"original contents").unwrap();

        let res = atomic_save_with_backup(
            &target,
            &backup_dir,
            20,
            |tmp| {
                fs::write(tmp, b"corrupt")?;
                Ok(())
            },
            |_tmp| bail!("simulated validation failure"),
        );

        assert!(res.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original contents");
        assert!(!root.join(".notes.txt.kbllm.tmp").exists());
        let backups: Vec<_> = fs::read_dir(&backup_dir).unwrap().collect();
        assert_eq!(backups.len(), 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn check_file_not_locked_detects_exclusive_handle() {
        let root = temp_dir("lock");
        let target = root.join("doc.txt");
        fs::write(&target, b"hello").unwrap();

        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&target)
            .unwrap();
        let rc = unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0);
        assert!(check_file_not_locked(&target).is_err());
        drop(held);
        assert!(check_file_not_locked(&target).is_ok());

        let _ = fs::remove_dir_all(&root);
    }
}
