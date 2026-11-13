//! Двуязычный интерфейс (русский / английский) без файлов перевода: строки пишутся парами прямо
//! в коде через `tr!("по-русски", "in English")`. Для строк с подстановкой —
//! `tr!(format!("… {x}"), format!("… {x}"))`: вычисляется только выбранная ветка.
//!
//! Язык: переменная `MCTUI_LANG` (ru/en) → поле `language` в servers.toml → язык системы.

use std::sync::atomic::{AtomicU8, Ordering};

const UNSET: u8 = 0;
const RU: u8 = 1;
const EN: u8 = 2;

static LANG: AtomicU8 = AtomicU8::new(UNSET);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Ru,
    En,
}

impl Lang {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        if s.starts_with("ru") {
            Some(Lang::Ru)
        } else if s.starts_with("en") {
            Some(Lang::En)
        } else {
            None
        }
    }
}

/// Язык системы: русский, если системная локаль русская (или украинская/белорусская/казахская), иначе английский.
pub fn system_lang() -> Lang {
    let loc = sys_locale::get_locale().unwrap_or_default().to_ascii_lowercase();
    if ["ru", "uk", "be", "kk"].iter().any(|p| loc.starts_with(p)) { Lang::Ru } else { Lang::En }
}

/// Выбор языка при старте: env → конфиг (`Some("ru" | "en")`, `None`/"auto" — системный).
pub fn init(config_language: Option<&str>) {
    let lang = std::env::var("MCTUI_LANG")
        .ok()
        .and_then(|v| Lang::parse(&v))
        .or_else(|| config_language.and_then(Lang::parse))
        .unwrap_or_else(system_lang);
    set(lang);
}

pub fn set(lang: Lang) {
    LANG.store(if lang == Lang::En { EN } else { RU }, Ordering::Relaxed);
}

#[cfg(test)]
thread_local! {
    static TEST_LANG: std::cell::Cell<Option<Lang>> = const { std::cell::Cell::new(None) };
}

/// Тесты: язык только для текущего потока. Тесты идут параллельно, а [`set`] меняет язык всего процесса.
#[cfg(test)]
pub fn with_lang<R>(lang: Lang, f: impl FnOnce() -> R) -> R {
    let prev = TEST_LANG.with(|c| c.replace(Some(lang)));
    let r = f();
    TEST_LANG.with(|c| c.set(prev));
    r
}

pub fn current() -> Lang {
    #[cfg(test)]
    if let Some(l) = TEST_LANG.with(|c| c.get()) {
        return l;
    }
    match LANG.load(Ordering::Relaxed) {
        EN => Lang::En,
        RU => Lang::Ru,
        _ => system_lang(),
    }
}

pub fn is_en() -> bool {
    current() == Lang::En
}

/// `tr!("Сервер запущен", "Server started")`
#[macro_export]
macro_rules! tr {
    ($ru:expr, $en:expr $(,)?) => {
        if $crate::i18n::is_en() { $en } else { $ru }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_switch() {
        assert_eq!(Lang::parse("ru_RU.UTF-8"), Some(Lang::Ru));
        assert_eq!(Lang::parse("EN"), Some(Lang::En));
        assert_eq!(Lang::parse("auto"), None);
        set(Lang::En);
        assert_eq!(tr!("да", "yes"), "yes");
        let n = 3;
        assert_eq!(tr!(format!("{n} шт"), format!("{n} pcs")), "3 pcs");
        set(Lang::Ru);
        assert_eq!(tr!("да", "yes"), "да");
    }
}
