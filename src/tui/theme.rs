//! Colours. Foregrounds use the terminal's own ANSI palette (e.g. anthropic-dark /
//! anthropic-light Ghostty themes), so text always matches the terminal. Backgrounds are
//! warm tints picked to sit on that palette's background at low contrast.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy)]
pub struct Theme {
    pub fg: Color,
    pub dim: Color,
    pub muted: Color,
    pub accent: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub blue: Color,
    pub magenta: Color,
    pub cyan: Color,
    pub inline_code: Color,
    pub user_bg: Color,
    pub sel_bg: Color,
    pub code_bg: Color,
    pub alert_bg: Color,
    pub warn_bg: Color,
    pub memory_bg: Color,
    pub message_bg: Color,
    pub add_bg: Color,
    pub del_bg: Color,
    pub panel_bg: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

impl Theme {
    /// For a #262624 background.
    pub fn dark() -> Self {
        Self {
            fg: Color::Reset,
            dim: Color::DarkGray,
            muted: Color::Gray,
            accent: rgb(0xD77757),
            red: Color::Red,
            green: Color::Green,
            yellow: Color::Yellow,
            blue: Color::Blue,
            magenta: Color::Magenta,
            cyan: Color::Cyan,
            inline_code: Color::LightYellow,
            user_bg: rgb(0x312F2B),
            sel_bg: rgb(0x45423B),
            code_bg: rgb(0x2D2B28),
            alert_bg: rgb(0x3D2926),
            warn_bg: rgb(0x37302A),
            memory_bg: rgb(0x243230),
            message_bg: rgb(0x2C2D37),
            add_bg: rgb(0x29342A),
            del_bg: rgb(0x3B2A28),
            panel_bg: rgb(0x2A2826),
        }
    }

    /// For a #F0EEE6 background.
    pub fn light() -> Self {
        Self {
            fg: Color::Reset,
            dim: Color::DarkGray,
            muted: Color::Gray,
            accent: rgb(0xB85C3C),
            red: Color::Red,
            green: Color::Green,
            yellow: Color::Yellow,
            blue: Color::Blue,
            magenta: Color::Magenta,
            cyan: Color::Cyan,
            inline_code: Color::Yellow,
            user_bg: rgb(0xE5E1D5),
            sel_bg: rgb(0xD8D2C2),
            code_bg: rgb(0xE8E4D9),
            alert_bg: rgb(0xF1DBD4),
            warn_bg: rgb(0xEEE3CD),
            memory_bg: rgb(0xDAE8E2),
            message_bg: rgb(0xE2E2EE),
            add_bg: rgb(0xDCEAD7),
            del_bg: rgb(0xF0DAD5),
            panel_bg: rgb(0xE9E6DC),
        }
    }

    pub fn dim(&self) -> Style {
        Style::new().fg(self.dim)
    }

    pub fn muted(&self) -> Style {
        Style::new().fg(self.muted)
    }

    pub fn accent(&self) -> Style {
        Style::new().fg(self.accent)
    }

    pub fn bold(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }
}
