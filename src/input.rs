//! Однострочное поле ввода с курсором (по символам, не байтам).

#[derive(Debug, Default, Clone)]
pub struct TextInput {
    pub value: String,
    cursor: usize,
}

impl TextInput {
    pub fn with(value: &str) -> Self {
        Self { value: value.to_string(), cursor: value.chars().count() }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn byte(&self, ci: usize) -> usize {
        self.value.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(self.value.len())
    }

    pub fn insert(&mut self, c: char) {
        let b = self.byte(self.cursor);
        self.value.insert(b, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars().filter(|c| !c.is_control()) {
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let b = self.byte(self.cursor);
            self.value.remove(b);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.value.chars().count() {
            let b = self.byte(self.cursor);
            self.value.remove(b);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.value.chars().count());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.value.chars().count();
    }

    pub fn set(&mut self, v: &str) {
        *self = Self::with(v);
    }

    pub fn take(&mut self) -> String {
        let v = std::mem::take(&mut self.value);
        self.cursor = 0;
        v
    }

    /// Обработка стандартных клавиш редактирования. Возвращает true, если клавиша поглощена.
    pub fn handle(&mut self, key: &crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                self.take();
            }
            KeyCode::Char('a') if ctrl => self.home(),
            KeyCode::Char('e') if ctrl => self.end(),
            KeyCode::Char(c) if !ctrl => self.insert(c),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Home => self.home(),
            KeyCode::End => self.end(),
            _ => return false,
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_editing() {
        let mut t = TextInput::default();
        for c in "say привет".chars() {
            t.insert(c);
        }
        t.backspace();
        t.left();
        t.insert('X');
        assert_eq!(t.value, "say привXе");
        t.home();
        t.delete();
        assert_eq!(t.value, "ay привXе");
    }
}
