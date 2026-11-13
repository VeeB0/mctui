pub mod layout;
mod logs;
mod main_screen;
mod panel;
mod popup;
mod setup;
#[cfg(all(test, not(windows)))]
mod tests;
pub mod theme;

use crate::app::{App, Screen};
use ratatui::Frame;

pub fn draw(app: &App, f: &mut Frame) {
    match app.screen {
        Screen::Main => main_screen::draw(app, f),
        Screen::Logs => logs::draw(app, f),
        Screen::Setup => setup::draw(app, f),
    }
    popup::draw(app, f);
}
