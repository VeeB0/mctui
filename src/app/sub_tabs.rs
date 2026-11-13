//! Общая таблица стрелок для вкладок с подвкладками: вкладки окна → подвкладки → записи.

use super::*;

/// Что сделала `sub_arrow` с клавишей.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Arrow {
    /// Клавиша не наша — её обработает общий код (←→ по вкладкам окна, ↑↓ по записям).
    NotMine,
    /// Клавиша съедена: уровень (и выбор) обновлены.
    Handled,
    /// Листнуть подвкладку на `d` (±1); сам список переключает вызывающий.
    Cycle(isize),
}

/// Переходы стрелок: вкладки окна → подвкладки → записи. `len` — число записей в текущей подвкладке.
pub(super) fn sub_arrow(level: &mut SubLevel, sel: &mut usize, len: usize, code: KeyCode) -> Arrow {
    use SubLevel::*;
    match (*level, code) {
        (Tabs, KeyCode::Down) => *level = Lists,
        (Tabs, KeyCode::Up) => {}
        (Tabs, _) => return Arrow::NotMine,
        (Lists, KeyCode::Up) => *level = Tabs,
        (Lists, KeyCode::Down) => {
            if len > 0 {
                *level = Entries;
                *sel = 0;
            }
        }
        (Entries, KeyCode::Up) if *sel == 0 => *level = Lists,
        (Lists | Entries, KeyCode::Left) => return Arrow::Cycle(-1),
        (Lists | Entries, KeyCode::Right) => return Arrow::Cycle(1),
        _ => return Arrow::NotMine,
    }
    Arrow::Handled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_arrow_walks_levels() {
        let (mut lv, mut sel) = (SubLevel::Tabs, 1usize);
        let go = |lv: &mut SubLevel, sel: &mut usize, len, code| sub_arrow(lv, sel, len, code);
        assert_eq!(go(&mut lv, &mut sel, 2, KeyCode::Down), Arrow::Handled);
        assert_eq!(lv, SubLevel::Lists);
        assert_eq!(go(&mut lv, &mut sel, 2, KeyCode::Right), Arrow::Cycle(1));
        assert_eq!(go(&mut lv, &mut sel, 2, KeyCode::Down), Arrow::Handled);
        assert_eq!((lv, sel), (SubLevel::Entries, 0));
        assert_eq!(go(&mut lv, &mut sel, 2, KeyCode::Down), Arrow::NotMine);
        assert_eq!(go(&mut lv, &mut sel, 2, KeyCode::Up), Arrow::Handled);
        assert_eq!(lv, SubLevel::Lists);
        assert_eq!(go(&mut lv, &mut sel, 2, KeyCode::Up), Arrow::Handled);
        assert_eq!(lv, SubLevel::Tabs);
        assert_eq!(go(&mut lv, &mut sel, 2, KeyCode::Up), Arrow::Handled);
        assert_eq!(lv, SubLevel::Tabs);
        assert_eq!(go(&mut lv, &mut sel, 2, KeyCode::Left), Arrow::NotMine);

        lv = SubLevel::Lists;
        assert_eq!(go(&mut lv, &mut sel, 0, KeyCode::Down), Arrow::Handled);
        assert_eq!(lv, SubLevel::Lists);
    }
}
