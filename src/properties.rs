//! server.properties с сохранением порядка строк и комментариев.

use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
enum Line {
    Raw(String),
    Pair { key: String, value: String },
}

#[derive(Clone, Debug, Default)]
pub struct Properties {
    lines: Vec<Line>,
    trailing_newline: bool,
}

impl Properties {
    pub fn parse(text: &str) -> Self {
        let lines = text
            .lines()
            .map(|l| {
                let t = l.trim_start();
                if t.is_empty() || t.starts_with('#') || t.starts_with('!') {
                    return Line::Raw(l.to_string());
                }
                match find_separator(l) {
                    Some(i) => {
                        Line::Pair { key: l[..i].trim().to_string(), value: l[i + 1..].trim_start().to_string() }
                    }
                    None => Line::Raw(l.to_string()),
                }
            })
            .collect();
        Self { lines, trailing_newline: text.ends_with('\n') }
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Ok(Self::parse(&std::fs::read_to_string(path)?))
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let tmp = path.with_extension("properties.tmp");
        std::fs::write(&tmp, self.render())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for (i, l) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            match l {
                Line::Raw(s) => out.push_str(s),
                Line::Pair { key, value } => {
                    out.push_str(key);
                    out.push('=');
                    out.push_str(value);
                }
            }
        }
        if self.trailing_newline && !self.lines.is_empty() {
            out.push('\n');
        }
        out
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.lines.iter().find_map(|l| match l {
            Line::Pair { key: k, value } if k == key => Some(value.as_str()),
            _ => None,
        })
    }

    pub fn set(&mut self, key: &str, val: &str) {
        for l in &mut self.lines {
            if let Line::Pair { key: k, value } = l
                && k == key
            {
                *value = val.to_string();
                return;
            }
        }
        self.lines.push(Line::Pair { key: key.to_string(), value: val.to_string() });
        self.trailing_newline = true;
    }

    pub fn pairs(&self) -> Vec<(String, String)> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                Line::Pair { key, value } => Some((key.clone(), value.clone())),
                _ => None,
            })
            .collect()
    }
}

/// Первый неэкранированный `=` или `:`.
fn find_separator(l: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in l.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '=' | ':' => return Some(i),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "#Minecraft server properties\n#Thu Nov 13 02:02:58 MSK 2026\nallow-flight=false\nmotd=A Minecraft Server\nrcon.password=\nlevel-seed=\n";

    #[test]
    fn roundtrip_is_exact() {
        assert_eq!(Properties::parse(SAMPLE).render(), SAMPLE);
        let no_nl = "a=1\n# c\nb=2";
        assert_eq!(Properties::parse(no_nl).render(), no_nl);
    }

    #[test]
    fn roundtrip_real_file_if_present() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../server.properties");
        if let Ok(text) = std::fs::read_to_string(p) {
            assert_eq!(Properties::parse(&text).render(), text);
        }
    }

    #[test]
    fn set_changes_only_one_line() {
        let mut p = Properties::parse(SAMPLE);
        p.set("motd", "Привет");
        let out = p.render();
        assert!(out.contains("motd=Привет\n"));
        assert!(out.starts_with("#Minecraft server properties\n"));
        assert_eq!(out.lines().count(), SAMPLE.lines().count());
        assert_eq!(p.get("allow-flight"), Some("false"));
        assert_eq!(p.get("rcon.password"), Some(""));
        p.set("new-key", "x");
        assert!(p.render().ends_with("new-key=x\n"));
    }
}
