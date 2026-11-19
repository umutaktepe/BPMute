use ratatui::style::{Color, Modifier, Style};

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

    pub fn from_env(ascii: bool) -> Self {
        let no_color = std::env::var("NO_COLOR").is_ok_and(|v| !v.trim().is_empty());
        Self::new(no_color, ascii)
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
}
