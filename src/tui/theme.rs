use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders};

use crate::ipc::protocol::AlertSeverity;
use crate::paths::EnvSource;
use crate::sleep_engine::StreamStatus;

const ASCII_BORDER_SET: symbols::border::Set<'static> = symbols::border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

/// 16-color ANSI theme that strictly respects `NO_COLOR` (https://no-color.org/).
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub no_color: bool,
    pub ascii: bool,
}

impl Theme {
    pub fn new(no_color: bool, ascii: bool) -> Self {
        Self { no_color, ascii }
    }

    pub fn from_env(env: &dyn EnvSource, ascii: bool) -> Self {
        let no_color = env.get("NO_COLOR").is_some_and(|v| !v.trim().is_empty());
        Self::new(no_color, ascii)
    }

    pub fn border_set(&self) -> symbols::border::Set<'static> {
        if self.ascii {
            ASCII_BORDER_SET
        } else {
            symbols::border::PLAIN
        }
    }

    pub fn card_block<'a>(&self, title: impl Into<Line<'a>>) -> Block<'a> {
        Block::default()
            .title(title)
            .title_style(self.card_title())
            .borders(Borders::ALL)
            .border_set(self.border_set())
            .border_style(self.card_border())
    }

    pub fn base(&self) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().fg(Color::White)
        }
    }

    pub fn primary(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
        }
    }

    pub fn secondary(&self) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().fg(Color::Blue)
        }
    }

    pub fn accent(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        }
    }

    pub fn success(&self) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().fg(Color::Green)
        }
    }

    pub fn warning(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        }
    }

    pub fn danger(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Red)
        }
    }

    pub fn muted(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }

    pub fn highlight(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        }
    }

    pub fn header_tab(&self, active: bool) -> Style {
        if active {
            if self.no_color {
                Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::White)
                    .add_modifier(Modifier::BOLD)
            }
        } else if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::Gray)
        }
    }

    pub fn card_border(&self) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }

    pub fn card_title(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub fn banner_grace(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightRed)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub fn banner_offline(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub fn banner_op_feedback(&self, is_error: bool) -> Style {
        if is_error {
            self.banner_grace()
        } else if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub fn alert_strip(&self, is_shift: bool) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else if is_shift {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::LightRed)
        }
    }

    pub fn key_hint(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub fn alert_border(&self, severity: AlertSeverity) -> Style {
        if self.no_color {
            Style::default()
        } else {
            match severity {
                AlertSeverity::Error => Style::default().fg(Color::Red),
                AlertSeverity::Warn => Style::default().fg(Color::Yellow),
                AlertSeverity::Info => Style::default().fg(Color::Cyan),
            }
        }
    }

    pub fn live_bpm(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub fn sleep_gauge(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(Color::Cyan)
        }
    }

    pub fn sparkline_curve(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Green)
        }
    }

    pub fn sparkline_qw_ref(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::Cyan)
        }
    }

    pub fn sparkline_sleep_ref(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::Magenta)
        }
    }

    pub fn table_header(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub fn stream_status(&self, status: StreamStatus) -> Style {
        match status {
            StreamStatus::Streaming => self.success(),
            StreamStatus::SignalLost => self.danger(),
            StreamStatus::PassiveStandby | StreamStatus::Acquiring | StreamStatus::Connecting => {
                self.warning()
            }
            StreamStatus::Disconnected => self.muted(),
        }
    }
}
