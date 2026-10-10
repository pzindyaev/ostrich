//! What the forms share: the VM name rule, the firmware / TPM / network
//! selectors and their labels, the bridge hint block, and the form chrome
//! (a bordered block with a step line, a focused label, help text, an error).

use anyhow::{bail, Result};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType};

use super::super::theme::Theme;
use super::super::widgets::wrap_text;
use crate::vm::{FirmwareType, NetworkType, VmConfig};

/// The network selector's entries, in order.
pub const NETWORK_CHOICES: [NetworkType; 3] =
    [NetworkType::User, NetworkType::Tap, NetworkType::None];
/// Their labels.
pub const NETWORK_LABELS: [&str; 3] = ["user (NAT)", "tap (bridge)", "none"];
/// The firmware selector's labels.
pub const FIRMWARE_LABELS: [&str; 3] = ["BIOS", "UEFI", "UEFI + Secure Boot"];
/// The TPM selector's labels.
pub const TPM_LABELS: [&str; 2] = ["disabled", "enabled"];

/// What a firmware selector index sets.
pub fn firmware_choice(index: usize) -> (FirmwareType, bool) {
    match index {
        1 => (FirmwareType::Uefi, false),
        2 => (FirmwareType::Uefi, true),
        _ => (FirmwareType::Bios, false),
    }
}

/// The firmware selector index matching a config.
pub fn firmware_index(cfg: &VmConfig) -> usize {
    if cfg.secure_boot {
        2
    } else if cfg.uefi() {
        1
    } else {
        0
    }
}

/// The network selector index for a type.
pub fn network_index(kind: NetworkType) -> usize {
    NETWORK_CHOICES.iter().position(|k| *k == kind).unwrap_or(0)
}

/// Checks that a VM name is non-empty and filesystem-safe. Errors:
/// `name cannot be empty`, `name may only contain letters, digits, hyphens and underscores`.
pub fn validate_vm_name(val: &str) -> Result<()> {
    if val.is_empty() {
        bail!("name cannot be empty");
    }
    if !val
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("name may only contain letters, digits, hyphens and underscores");
    }
    Ok(())
}

/// The setup hint to show next to a network choice: only tap networking
/// asks anything of the host, and `""` means it is all there.
pub fn bridge_hint_for(kind: NetworkType) -> String {
    if kind != NetworkType::Tap {
        return String::new();
    }
    crate::vm::bridge_hint(crate::vm::BRIDGE_NAME)
}

/// The hint as warning lines, `⚠ ` before the first and two spaces of
/// indent on the rest, each wrapped to `width`. Empty for an empty hint.
pub fn bridge_hint_lines(hint: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    if hint.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, raw) in hint.lines().enumerate() {
        let prefix = if i == 0 { "⚠ " } else { "  " };
        let avail = width.saturating_sub(2).max(8);
        for (j, piece) in wrap_text(raw, avail).into_iter().enumerate() {
            let p = if j == 0 { prefix } else { "  " };
            out.push(Line::styled(format!("{p}{piece}"), theme.warn));
        }
    }
    out
}

/// Draws the frame every form sits in: a rounded bordered block titled
/// `title` (focused border style), and returns the inner area with a one-cell
/// horizontal margin.
pub fn form_block(frame: &mut Frame, area: Rect, title: &str, theme: &Theme) -> Rect {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border_focused)
        .title(Line::styled(format!(" {title} "), theme.title));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner.inner(Margin::new(1, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_names() {
        assert!(validate_vm_name("debian-12_x").is_ok());
        assert_eq!(
            validate_vm_name("").unwrap_err().to_string(),
            "name cannot be empty"
        );
        assert_eq!(
            validate_vm_name("a b").unwrap_err().to_string(),
            "name may only contain letters, digits, hyphens and underscores"
        );
        assert!(validate_vm_name("a/b").is_err());
        assert!(validate_vm_name("ä").is_err());
    }

    #[test]
    fn firmware_round_trip() {
        for i in 0..3 {
            let (fw, sb) = firmware_choice(i);
            let cfg = VmConfig {
                firmware: fw,
                secure_boot: sb,
                ..Default::default()
            };
            assert_eq!(firmware_index(&cfg), i);
        }
        assert_eq!(network_index(NetworkType::Tap), 1);
    }
}
