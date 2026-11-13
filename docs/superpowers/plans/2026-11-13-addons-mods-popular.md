# Моды по ядру и популярное в поиске — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** первая подвкладка «Дополнений» называется по ядру сервера («Моды» на Fabric и т. п.), а поиск дополнений
сразу показывает 10 самых скачиваемых, `n` — следующие 10.

**Architecture:** `plugin_search` переходит со смещения на номер страницы по 10 строк (у двух источников — 5+5
вперемешку) и сортирует пустой запрос по скачиваниям; панель поиска запускает пустой запрос при открытии;
`refresh_plugins` запоминает ядро, а отрисовка подписи и пустого списка берёт его оттуда.

**Tech Stack:** Rust 2024, ratatui 0.30, tokio; HTTP через трейт `Http` (`Curl` в работе, `FakeHttp` в тестах).

**Spec:** `docs/superpowers/specs/2026-11-13-addons-mods-popular-design.md`

## Global Constraints
- Весь текст интерфейса — через `tr!("ru","en")`, обе строки.
- Страница поиска — 10 строк (`PAGE_SIZE = 10`); у двух источников — по 5 из каждого, порядок M, H, M, H…
- Пустой запрос: Modrinth `index=downloads`, Hangar `sort=-downloads`. Непустой — Modrinth `index=relevance`,
  Hangar без `sort`.
- Подписи подвкладки: «Моды (N)»/"Mods (N)" (Fabric, Quilt, Forge, NeoForge), «Плагины (N)»/"Plugins (N)"
  (Paper, Purpur, Spigot, Folia, неизвестное ядро), «Плагины и моды (N)»/"Plugins and mods (N)" — если в списке
  есть оба вида.
- Ваниль: строка «Ванильное ядро не поддерживает плагины и моды — только датапаки» /
  "The vanilla core supports no plugins or mods — only datapacks"; `i` на подвкладке — тост с тем же текстом.
- Моддовое ядро, пустой список: «Модов нет · i — найти и установить» / "No mods · i to find and install".
- Определение ядра — только при перечитывании списка, не при отрисовке.
- Тесты не ходят в сеть.
- `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets` — чисто.

## Review Focus
1. Hangar недоступен у Paper → страница из 5 строк Modrinth, предупреждение в тосте, `n` работает дальше.
2. Повторное открытие поиска или быстрые `n` подряд не дублируют строки (дедуп по `source`+`id` уже есть).
3. Запрос введён, пока грузится популярное → результат популярного не должен затереть выдачу по запросу.
4. Сервер с папкой `plugins/` и jar Fabric → подпись «Моды», если в списке только моды.
5. Ваниль без единого мода/плагина — строка про ваниль, а не «Плагинов нет · i — найти».

---

### Task 1: Поиск — страницы по 10, сортировка по скачиваниям, M/H вперемешку

**Files:** Modify `src/plugin_search.rs`

**Interfaces:**
- Produces:
  ```rust
  pub const PAGE_SIZE: usize = 10;
  /// `page` — номер страницы с 0. У двух источников каждый отдаёт PAGE_SIZE / 2 со смещением page * (PAGE_SIZE / 2).
  pub fn search(query: &str, target: &Target, page: usize) -> Result<SearchResults>;
  pub(crate) fn search_with(http: &dyn Http, query: &str, target: &Target, page: usize) -> Result<SearchResults>;
  ```
  `SearchResults.more` — есть ли следующая страница хотя бы у одного источника.

- [ ] **Step 1: Тесты** в `plugin_search::tests`:
  - `empty_query_sorts_by_downloads`: `FakeHttp` с маршрутами Modrinth и Hangar; `search_with(&http, "", &paper(Some("26.3")), 0)`
    → URL Modrinth содержит `index=downloads` и `limit=5&offset=0`, URL Hangar содержит `sort=-downloads` и
    `limit=5&offset=0`; `search_with(&http, "chunky", …, 1)` → `index=relevance`, `offset=5`, в URL Hangar нет `sort=`.
  - `single_source_pages_by_ten`: `Target::new(Platform::Fabric, None)`, страница 2 → `limit=10&offset=20`, Hangar не спрашивается.
  - `hits_alternate_sources`: Modrinth отдаёт A, B, C, Hangar — X, Y → порядок A, X, B, Y, C; дубль Hangar по имени
    (как в `merge_hits` сейчас) выпадает, порядок остальных сохраняется.
  - Поправить `search_partial_failure` и живой тест (строка с `search("chunky", &paper, PAGE_SIZE)`) под номер страницы.
- [ ] **Step 2:** `cargo test plugin_search` → новые тесты FAIL.
- [ ] **Step 3:** реализовать: `PAGE_SIZE = 10`; число источников = 2, если `target.platform.hangar().is_some()`,
  иначе 1; `per = PAGE_SIZE / sources`, `offset = page * per`; `limit=per` в обоих URL; `more |= offset + per < total`;
  `merge_hits` → чередование с прежним дедупом; док-комментарии модуля и `search`.
- [ ] **Step 4:** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: поиск дополнений — популярное по скачиваниям, страницы по 10`.

### Task 2: Панель поиска сразу показывает популярное

**Files:** Modify `src/app/panel.rs`, `src/ui/tests.rs`

**Interfaces:**
- Consumes: Task 1 (`search(query, target, page)`).
- `PanelKind::PluginSearch { target, query, hits, page: usize, more }` (поле `offset` → `page`).
- Применение ответа выносится в метод, который тесты зовут напрямую:
  ```rust
  pub(super) fn apply_plugin_search(&mut self, query: &str, page: usize, res: anyhow::Result<ps::SearchResults>);
  ```

- [ ] **Step 1: Тесты** в `ui::tests`:
  - `addon_search_loads_popular_on_open`: `TempServer` + `paper.jar` в корне, `4`, `i` → панель открыта, `busy`
    = `searching…` (En), строка ввода пустая и в фокусе.
  - `addon_search_ignores_stale_popular`: открыть, ввести `chunky`, Enter, затем
    `a.apply_plugin_search("", 0, Ok(<SearchResults с одной строкой "Old">))` → в панели нет строки «Old»;
    `a.apply_plugin_search("chunky", 0, Ok(<строка "Chunky">))` → строка есть.
  - `datapack_search_works_without_core` остаётся зелёным; в нём строка-подсказка — новая (Step 3).
- [ ] **Step 2:** `cargo test addon_search` → FAIL.
- [ ] **Step 3:** реализовать:
  - в `open_addon_search` после создания панели — `run_plugin_search(&mut panel, false)` до `self.popup = Some(…)`;
  - `n` → `page += 1`; новый запрос → `page = 0`;
  - ответ применяется, только если `query` и `page` в панели совпадают с теми, с которыми он запрошен;
  - подсказка: «Сейчас — популярное. Введите название и нажмите Enter.» /
    "Showing popular ones. Type a name and press Enter." + прежняя строка источников;
  - сеть в тестах: под `#[cfg(test)]` работа поиска не вызывает `ps::search`, а возвращает пустой `SearchResults`
    (одна функция-обёртка в `panel.rs`), чтобы UI-тесты не ходили в интернет.
- [ ] **Step 4:** `cargo test` → PASS; `cargo fmt --check`, `cargo clippy --all-targets`.
- [ ] **Step 5: Commit** `feat: поиск дополнений сразу показывает популярное`.

### Task 3: Подвкладка по ядру, ваниль и пустой список модов

**Files:** Modify `src/app.rs` (`PluginsView`), `src/app/plugins_tab.rs`, `src/app/keys.rs` или `panel.rs`
(где `i` открывает поиск), `src/ui/main_screen.rs`, `src/ui/tests.rs`, `README.md`, `CHANGELOG.md`

**Interfaces:**
- `PluginsView` получает поле ядра, например `core: Option<AddonsCore>`:
  ```rust
  pub enum AddonsCore { Plugins, Mods, Vanilla }
  ```
  Заполняется в `refresh_plugins`: `Platform::detect(dir, &command.join(" "))` → `Plugins`/`Mods` по `kind()`;
  `None` и `core_update::detect_with(dir, &command)` с `Core::Vanilla` → `Vanilla`; иначе `None`.

- [ ] **Step 1: Тесты** в `ui::tests`:
  - `fabric_without_mods_is_called_mods`: `TempServer` + `fabric-server-launch.jar` в корне, пустая `mods/` → подпись
    подвкладки `Mods (0)`, в теле `No mods · i to find and install`.
  - `paper_is_called_plugins`: `paper.jar`, пустая `plugins/` → `Plugins (0)`.
  - `vanilla_says_only_datapacks`: `server.jar` с `version.json` (как в `core_update::tests::detect_layouts`) →
    тело содержит `The vanilla core supports no plugins or mods — only datapacks`; `i` → тост с этим текстом, `popup` нет.
  - `datapack_search_works_without_core` — сервер без jar остаётся «неизвестным ядром»: `i` на плагинах не открывает панель.
- [ ] **Step 2:** `cargo test fabric_without paper_is vanilla_says` (через `--`) → FAIL.
- [ ] **Step 3:** реализовать; подпись — по `core`, «Плагины и моды» — если в `items` оба вида (перекрывает ядро);
  `PECHKA_UPDATE_SNAPSHOTS=1 cargo test ui::tests`, просмотреть изменения снимков.
  README — абзац «Дополнения»: подвкладка называется по ядру, поиск сразу показывает популярное.
  CHANGELOG `[Unreleased]`: «Изменено» — оба пункта.
- [ ] **Step 4:** `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets` → чисто.
- [ ] **Step 5: Commit** `feat: подвкладка «Моды» на моддовых ядрах, пояснение для ванили`.
