//! Application-level settings, persisted to `~/.config/ostrich/config.json`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::vm::config::{null_as_default, null_items_as_default, write_file_atomic};

/// How many image paths the config remembers; the oldest go first.
pub const MAX_RECENT_ISOS: usize = 20;

/// Settings that belong to the application rather than to one VM.
///
/// A hand-edited file loads the way Go's encoding/json read it: a missing
/// key or a `null` is the zero value. One without a VM storage path counts
/// as no config at all, though (see [`load`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    /// The directory every VM gets its own sub-folder in.
    #[serde(default, deserialize_with = "null_as_default")]
    pub vm_storage_path: String,
    /// Image paths used before (boot ISOs and USB images), newest first. The
    /// ISO picker offers them.
    #[serde(
        default,
        deserialize_with = "null_items_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub recent_isos: Vec<String>,
}

/// `~/.config/ostrich/config.json`, with `~` being `$HOME`.
pub fn config_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".config").join("ostrich").join("config.json")
}

/// Reads and parses the app config. `Ok(None)` means there is no config yet,
/// the first run: there is no file, or its `vm_storage_path` is missing,
/// null or blank, which would put the VMs in whatever directory Ostrich was
/// started from. The setup screen then asks for one.
pub fn load() -> Result<Option<AppConfig>> {
    load_at(&config_path())
}

/// [`load`] from an explicit path.
pub fn load_at(path: &Path) -> Result<Option<AppConfig>> {
    Ok(read_at(path)?.filter(|cfg| !cfg.vm_storage_path.trim().is_empty()))
}

/// The config file as it is, storage path or not; `Ok(None)` when there is
/// no file. The remembered image paths are read through this, so those of a
/// file without a storage path still count.
fn read_at(path: &Path) -> Result<Option<AppConfig>> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let cfg: AppConfig =
        serde_json::from_slice(&data).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(cfg))
}

/// Writes the config to disk, creating parent directories as needed. The
/// file is replaced atomically, so it is never left cut short.
pub fn save(cfg: &AppConfig) -> Result<()> {
    save_at(&config_path(), cfg)
}

/// [`save`] to an explicit path.
pub fn save_at(path: &Path, cfg: &AppConfig) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let data = serde_json::to_string_pretty(cfg)?;
    write_file_atomic(path, data.as_bytes()).with_context(|| format!("write {}", path.display()))
}

/// The image paths used before, newest first; none when there is no config
/// to read.
pub fn recent_isos() -> Vec<String> {
    recent_isos_at(&config_path())
}

/// [`recent_isos`] from an explicit config path.
pub fn recent_isos_at(path: &Path) -> Vec<String> {
    match read_at(path) {
        Ok(Some(cfg)) => cfg.recent_isos,
        _ => Vec::new(),
    }
}

/// Puts `iso` at the top of the remembered image paths and saves the config.
/// A path already there moves up; the oldest beyond [`MAX_RECENT_ISOS`] are
/// dropped. Fails when there is no config yet.
pub fn remember_iso(iso: &str) -> Result<()> {
    remember_iso_at(&config_path(), iso)
}

/// [`remember_iso`] on an explicit config path.
pub fn remember_iso_at(path: &Path, iso: &str) -> Result<()> {
    let mut cfg = read_at(path)?.context("config not found")?;
    let mut recent = Vec::with_capacity(cfg.recent_isos.len() + 1);
    recent.push(iso.to_string());
    recent.extend(without(&cfg.recent_isos, iso));
    recent.truncate(MAX_RECENT_ISOS);
    cfg.recent_isos = recent;
    save_at(path, &cfg)
}

/// Drops `iso` from the remembered image paths and saves the config.
pub fn forget_iso(iso: &str) -> Result<()> {
    forget_iso_at(&config_path(), iso)
}

/// [`forget_iso`] on an explicit config path.
pub fn forget_iso_at(path: &Path, iso: &str) -> Result<()> {
    let mut cfg = read_at(path)?.context("config not found")?;
    cfg.recent_isos = without(&cfg.recent_isos, iso);
    save_at(path, &cfg)
}

/// `paths` with every occurrence of `path` left out.
fn without(paths: &[String], path: &str) -> Vec<String> {
    paths
        .iter()
        .filter(|p| p.as_str() != path)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(".config")
            .join("ostrich")
            .join("config.json");
        (dir, path)
    }

    #[test]
    fn load_reports_first_run_when_missing() {
        let (_dir, path) = temp_config();
        assert_eq!(load_at(&path).unwrap(), None);
        assert!(recent_isos_at(&path).is_empty());
    }

    #[test]
    fn save_creates_directories_and_round_trips() {
        let (_dir, path) = temp_config();
        let cfg = AppConfig {
            vm_storage_path: "/home/user/VMs".into(),
            recent_isos: vec!["/iso/a.iso".into()],
        };
        save_at(&path, &cfg).unwrap();
        assert_eq!(load_at(&path).unwrap(), Some(cfg));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"vm_storage_path\": \"/home/user/VMs\""));
        assert!(text.contains("\"recent_isos\""));
    }

    #[test]
    fn recent_isos_key_is_omitted_when_empty() {
        let (_dir, path) = temp_config();
        save_at(
            &path,
            &AppConfig {
                vm_storage_path: "/x".into(),
                recent_isos: vec![],
            },
        )
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("recent_isos"), "{text}");
        // And a config written before the field existed still loads.
        fs::write(&path, "{\"vm_storage_path\": \"/y\"}").unwrap();
        assert_eq!(load_at(&path).unwrap().unwrap().vm_storage_path, "/y");
    }

    #[test]
    fn remember_moves_to_top_dedups_and_caps() {
        let (_dir, path) = temp_config();
        save_at(
            &path,
            &AppConfig {
                vm_storage_path: "/x".into(),
                recent_isos: vec![],
            },
        )
        .unwrap();
        remember_iso_at(&path, "/a").unwrap();
        remember_iso_at(&path, "/b").unwrap();
        remember_iso_at(&path, "/a").unwrap();
        assert_eq!(
            recent_isos_at(&path),
            vec!["/a".to_string(), "/b".to_string()]
        );
        for i in 0..30 {
            remember_iso_at(&path, &format!("/iso{i}")).unwrap();
        }
        let got = recent_isos_at(&path);
        assert_eq!(got.len(), MAX_RECENT_ISOS);
        assert_eq!(got[0], "/iso29");
        assert_eq!(got[MAX_RECENT_ISOS - 1], "/iso10");
    }

    #[test]
    fn forget_drops_every_occurrence_and_keeps_the_rest() {
        let (_dir, path) = temp_config();
        save_at(
            &path,
            &AppConfig {
                vm_storage_path: "/x".into(),
                recent_isos: vec!["/a".into(), "/b".into(), "/a".into()],
            },
        )
        .unwrap();
        forget_iso_at(&path, "/a").unwrap();
        assert_eq!(recent_isos_at(&path), vec!["/b".to_string()]);
        forget_iso_at(&path, "/nope").unwrap();
        assert_eq!(recent_isos_at(&path), vec!["/b".to_string()]);
    }

    #[test]
    fn hand_edited_missing_keys_and_nulls_load_as_zero_values() {
        // encoding/json left the zero value for a missing key or a null.
        let (_dir, path) = temp_config();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for (json, want) in [
            (
                "{\"vm_storage_path\": \"/x\"}",
                AppConfig {
                    vm_storage_path: "/x".into(),
                    recent_isos: vec![],
                },
            ),
            (
                "{\"vm_storage_path\": \"/x\", \"recent_isos\": null}",
                AppConfig {
                    vm_storage_path: "/x".into(),
                    recent_isos: vec![],
                },
            ),
            (
                "{\"vm_storage_path\": \"/x\", \"recent_isos\": [\"/a\", null, \"/b\"]}",
                AppConfig {
                    vm_storage_path: "/x".into(),
                    recent_isos: vec!["/a".into(), String::new(), "/b".into()],
                },
            ),
        ] {
            fs::write(&path, json).unwrap();
            assert_eq!(load_at(&path).unwrap(), Some(want), "{json}");
        }
        // Wrong types are still an error.
        fs::write(&path, "{\"vm_storage_path\": 5}").unwrap();
        assert!(load_at(&path).is_err());
    }

    /// Go took a missing, null or blank storage path for the current
    /// directory and kept VMs wherever Ostrich was started; here it is no
    /// config yet, and the setup screen asks for one. The rest of the file
    /// reads as before.
    #[test]
    fn a_blank_storage_path_is_no_config_yet() {
        let (_dir, path) = temp_config();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for json in [
            "{}",
            "{\"vm_storage_path\": null}",
            "{\"vm_storage_path\": \"\"}",
            "{\"vm_storage_path\": \" \\t \"}",
            "{\"recent_isos\": [\"/a\"]}",
        ] {
            fs::write(&path, json).unwrap();
            assert_eq!(load_at(&path).unwrap(), None, "{json}");
        }
        // Its remembered images are still offered and kept.
        fs::write(
            &path,
            "{\"vm_storage_path\": \"\", \"recent_isos\": [\"/a\", null]}",
        )
        .unwrap();
        assert_eq!(recent_isos_at(&path), ["/a".to_string(), String::new()]);
        remember_iso_at(&path, "/b").unwrap();
        forget_iso_at(&path, "").unwrap();
        assert_eq!(recent_isos_at(&path), ["/b".to_string(), "/a".to_string()]);
        assert_eq!(load_at(&path).unwrap(), None);
        // A path with spaces around it is a path, and a broken file is still
        // an error rather than a first run.
        fs::write(&path, "{\"vm_storage_path\": \" /x \"}").unwrap();
        assert_eq!(load_at(&path).unwrap().unwrap().vm_storage_path, " /x ");
        fs::write(&path, "{\"vm_storage_path\": 5}").unwrap();
        assert!(load_at(&path).is_err());
        fs::write(&path, "{").unwrap();
        assert!(load_at(&path).is_err());
    }

    #[test]
    fn save_replaces_the_file_atomically_and_writes_through_a_symlink() {
        let (dir, path) = temp_config();
        let cfg = AppConfig {
            vm_storage_path: "/vms".into(),
            recent_isos: vec![],
        };
        save_at(&path, &cfg).unwrap();
        save_at(&path, &cfg).unwrap();
        // No temporary file is left next to it.
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.json"]);

        // A config.json linked from elsewhere (a dotfiles repository) stays a
        // link; the file it points at is the one rewritten.
        let real = dir.path().join("dotfiles-config.json");
        fs::rename(&path, &real).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        let moved = AppConfig {
            vm_storage_path: "/elsewhere".into(),
            recent_isos: vec!["/a.iso".into()],
        };
        save_at(&path, &moved).unwrap();
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(load_at(&real).unwrap(), Some(moved));
    }

    #[test]
    fn remember_and_forget_need_a_config() {
        let (_dir, path) = temp_config();
        assert!(remember_iso_at(&path, "/a").is_err());
        assert!(forget_iso_at(&path, "/a").is_err());
    }
}
