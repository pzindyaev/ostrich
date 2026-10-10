//! The colour palette and the named styles every screen draws with. One
//! place to change, so the dashboard reads as one surface.

use ratatui::style::{Color, Modifier, Style};

/// The named styles of the UI.
#[derive(Debug, Clone)]
pub struct Theme {
    pub primary: Color,
    pub accent: Color,
    pub danger: Color,
    pub warning: Color,
    pub muted: Color,
    pub text: Color,

    /// Pane and screen titles.
    pub title: Style,
    /// Secondary text under a title.
    pub subtitle: Style,
    /// `● running`.
    pub running: Style,
    /// `● stopped`.
    pub stopped: Style,
    /// Border of the pane that has the keyboard focus.
    pub border_focused: Style,
    /// Border of every other pane.
    pub border: Style,
    /// Error text (`✗ ...`).
    pub error: Style,
    /// Warning text (`⚠ ...`).
    pub warn: Style,
    /// Success text (`✓ ...`).
    pub success: Style,
    /// Key hints and other dim helper text.
    pub help: Style,
    /// Field labels and the focused row marker.
    pub label: Style,
    /// The row under the cursor in a focused list, and the chosen entry of a selector.
    pub selected: Style,
    /// The row under the cursor in a list that does not have the focus.
    pub selected_unfocused: Style,
    /// Plain body text.
    pub normal: Style,
    /// The key part of a key hint (`j/k`).
    pub hint_key: Style,
    /// The description part of a key hint (`move`).
    pub hint_desc: Style,
    /// A text input's contents.
    pub input: Style,
    /// A text input's placeholder.
    pub placeholder: Style,
    /// The spinner glyph.
    pub spinner: Style,
}

impl Default for Theme {
    fn default() -> Self {
        let primary = Color::Rgb(0x7C, 0x3A, 0xED); // purple
        let accent = Color::Rgb(0x10, 0xB9, 0x81); // green
        let danger = Color::Rgb(0xEF, 0x44, 0x44); // red
        let warning = Color::Rgb(0xF5, 0x9E, 0x0B); // amber
        let muted = Color::Rgb(0x6B, 0x72, 0x80); // gray
        let text = Color::Rgb(0xF3, 0xF4, 0xF6); // near-white
        Theme {
            primary,
            accent,
            danger,
            warning,
            muted,
            text,
            title: Style::new().fg(primary).add_modifier(Modifier::BOLD),
            subtitle: Style::new().fg(muted).add_modifier(Modifier::ITALIC),
            running: Style::new().fg(accent).add_modifier(Modifier::BOLD),
            stopped: Style::new().fg(danger),
            border_focused: Style::new().fg(primary),
            border: Style::new().fg(muted),
            error: Style::new().fg(danger),
            warn: Style::new().fg(warning),
            success: Style::new().fg(accent),
            help: Style::new().fg(muted),
            label: Style::new().fg(primary).add_modifier(Modifier::BOLD),
            selected: Style::new()
                .fg(text)
                .bg(primary)
                .add_modifier(Modifier::BOLD),
            selected_unfocused: Style::new().fg(text).bg(Color::Rgb(0x37, 0x41, 0x51)),
            normal: Style::new().fg(text),
            hint_key: Style::new().fg(text).add_modifier(Modifier::BOLD),
            hint_desc: Style::new().fg(muted),
            input: Style::new().fg(text),
            placeholder: Style::new().fg(muted).add_modifier(Modifier::ITALIC),
            spinner: Style::new().fg(primary).add_modifier(Modifier::BOLD),
        }
    }
}
