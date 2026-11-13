# Вкладка «Дополнения» с датапаками — план реализации

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** вкладка 4 становится «Дополнениями» с подвкладками «Плагины» и «Датапаки»; датапаки текущего мира
можно смотреть, включать/выключать файлами, обновлять и ставить с Modrinth.

**Architecture:** новый модуль `src/datapacks.rs` (скан, перенос, признаки содержимого, обновления) по образцу
`plugins.rs`. Поиск/установка переиспользуют `plugin_search.rs` через псевдо-ядро `Platform::Datapack`
и `PluginKind::Datapack` (база установки — папка мира, а не сервера). Уровни ↓/↑ выносятся из «Доступа»
в общий `SubLevel` + `sub_arrow`, которыми пользуются обе вкладки.

**Tech Stack:** Rust 2024, ratatui 0.30, crossterm 0.29, tokio; уже есть `zip`, `flate2`, `serde_json`, `sha1`.

**Spec:** `docs/superpowers/specs/2026-11-13-datapacks-design.md` — читать вместе с планом.

## Global Constraints

- Перед Task 1: ветка `feature/datapacks` от текущего `main` (незакоммиченные правки вкладки «Доступ» уходят в неё).
  Шаги «Commit» выполнять, только если пользователь согласился на коммиты; иначе пропускать.
- Тексты только через `crate::tr!("ru", "en")`; хоткеи проходят через `app::latin()` (х → `[`, ъ → `]`).
- Новых зависимостей не добавлять.
- Папки (относительно `<сервер>/<level-name>`, level-name — `world::current_level_name(dir)`):
  включённые `datapacks/`, выключенные `datapacks.disabled/`, старые версии `datapacks.disabled/.pechka-old/`,
  временные загрузки — `datapacks.disabled/.pechka-download-<pid>.part`. В `datapacks/` pechka не кладёт
  ничего, кроме датапаков.
- **Никогда** не писать `level.dat`, не удалять файлы датапаков (только переносить), не отправлять `reload`.
  Перезагрузка датапаков — консольная команда ровно `minecraft:reload`; включить выключенный в игре —
  `datapack enable "file/<имя файла>"`.
- Ошибка переноса: `не удалось перенести <имя>: остановите сервер и повторите` /
  `failed to move <name>: stop the server and try again`.
- Название вкладки 4: «Дополнения» / "Add-ons"; подвкладки «Плагины|Моды|Плагины и моды (N)» и «Датапаки (N)» /
  "Plugins|Mods|Plugins and mods (N)", "Datapacks (N)". Клавиша `4` та же.
- Правило стрелок: попав на вкладку, ←→ листают вкладки окна; ↓ — к подвкладкам, ещё ↓ — к записям, ↑ — обратно.
- Тесты: язык — `i18n::with_lang`, снимки — `PECHKA_UPDATE_SNAPSHOTS=1 cargo test ui::tests`;
  ручные проверки — только на песочнице в scratchpad, никогда на рабочем сервере пользователя.
- В конце каждой задачи: `cargo fmt --check`, `cargo clippy --all-targets` без предупреждений, `cargo test` зелёный.

## Review Focus

1. **Мира ещё нет** (сервер ни разу не запускался): вкладка показывает пустой список без ошибок, установка
   создаёт `<мир>/datapacks/`. Тесты — Task 2 (`scan_missing_world_is_empty`) и Task 4 (`install_datapack_creates_world_dirs`).
2. **Одноимённый файл в обеих папках** (`X.zip` и в `datapacks/`, и в `datapacks.disabled/`): переключение
   и установка дают ошибку и ничего не затирают. Тесты — Task 2 (`set_enabled_refuses_to_overwrite`),
   Task 4 (`install_datapack_refuses_disabled_twin`).
3. **На сервере есть игроки:** вместо мгновенного `minecraft:reload` — подтверждение со списком игроков.
   Тест — Task 6 (`datapack_toggle_with_players_asks_before_reload`).
4. **Сломанный датапак** (битый zip, `pack.mcmeta` с невалидным JSON, описание-компонент с `extra`/массивом,
   огромный `pack.mcmeta`): строка в списке есть, описание пустое/плоский текст, «нужен перезапуск» = true
   для нечитаемого zip. Тест — Task 2 (`scan_survives_broken_packs`, `description_flattens_components`).
5. **RCON-сервер на «Дополнениях»:** уровни не включаются, ←→ листают вкладки окна, без паники.
   Тест — Task 5 (`addons_on_rcon_server_arrows_switch_tabs`).

---

### Task 1: Общие уровни подвкладок (`SubLevel`, `sub_arrow`, строка подвкладок)

**Files:**
- Modify: `src/app.rs` (enum `AccessLevel` → `SubLevel`, поле `AccessView.level: SubLevel`)
- Create: `src/app/sub_tabs.rs` (зарегистрировать `mod sub_tabs;` рядом с остальными подмодулями `app`)
- Modify: `src/app/access_tab.rs` (`access_arrow` поверх `sub_arrow`), `src/app/keys.rs`, `src/ui/main_screen.rs`, `src/ui/tests.rs`

**Interfaces:**
- Produces:
  - `pub enum SubLevel { #[default] Tabs, Lists, Entries }` в `app.rs` (переименование `AccessLevel` через
    Serena `rename_symbol`; варианты и derive те же).
  - `pub(super) enum Arrow { NotMine, Handled, Cycle(isize) }` и
    `pub(super) fn sub_arrow(level: &mut SubLevel, sel: &mut usize, len: usize, code: KeyCode) -> Arrow`
    в `app/sub_tabs.rs` — та же таблица переходов, что сейчас в `access_arrow` (Tabs: ↓→Lists, ↑ съедается,
    ←→ NotMine; Lists: ↑→Tabs, ↓→Entries при `len > 0` с `sel = 0`, ←→ Cycle; Entries: ↑ при `sel == 0`→Lists,
    ↑↓ иначе NotMine, ←→ Cycle).
  - `pub fn sub_tabs_line(labels: &[String], active: usize, level: SubLevel, focused: bool) -> Line<'static>`
    в `src/ui/main_screen.rs` — строка подвкладок со стилями и подсказкой справа, как сейчас у «Доступа».

- [ ] **Step 1: Тест на чистую функцию** — в `app/sub_tabs.rs` `#[cfg(test)] mod tests`, тест
  `sub_arrow_walks_levels`, при `len = 2`, начиная с `Tabs`, `sel = 1`:
  Tabs+↓ → `Handled`, Lists; Lists+→ → `Cycle(1)`; Lists+↓ → `Handled`, Entries, `sel == 0`;
  Entries+↓ → `NotMine`; Entries+↑ (sel 0) → `Handled`, Lists; Lists+↑ → `Handled`, Tabs; Tabs+↑ → `Handled`, Tabs;
  Tabs+← → `NotMine`. При `len = 0`: Lists+↓ → `Handled`, уровень остаётся Lists.
- [ ] **Step 2:** `cargo test sub_arrow_walks_levels` → FAIL (нет модуля).
- [ ] **Step 3:** реализовать `sub_arrow`, переименовать `AccessLevel` → `SubLevel`, переписать
  `access_arrow` на `sub_arrow` (Cycle → `cycle_access_list`), `draw_access` — на `sub_tabs_line`.
- [ ] **Step 4:** `cargo test` → всё зелёное, включая `arrows_switch_tabs_on_access_tab` и
  `access_tab_down_enters_lists_up_leaves` (поведение «Доступа» не меняется).
- [ ] **Step 5: Commit** `refactor: общие уровни подвкладок для вкладок с подсписками`.

---

### Task 2: `src/datapacks.rs` — скан, перенос, признаки содержимого

**Files:**
- Create: `src/datapacks.rs` (+ `mod datapacks;` в `src/main.rs`)
- Test: там же, `#[cfg(test)] mod tests` (временные папки как в `plugin_search.rs::tests::TempDir`; zip — через `zip::ZipWriter`;
  `level.dat` — по образцу `world.rs::tests::level_dat`)

**Interfaces:**
- Consumes: `world::current_level_name`, `world::nbt::{read_file, Tag}`, `plugins::sha1_file`.
- Produces:
  ```rust
  pub const ENABLED_DIR: &str = "datapacks";
  pub const DISABLED_DIR: &str = "datapacks.disabled";
  pub const DATAPACK_LOADERS: &[&str] = &["datapack"];

  #[derive(Debug, Clone, PartialEq)]
  pub struct DatapackInfo {
      pub path: PathBuf,           // где лежит сейчас
      pub file_name: String,       // "Terralith.zip" или имя папки
      pub name: String,            // file_name без ".zip"
      pub is_zip: bool,
      pub enabled: bool,           // лежит в datapacks/
      pub disabled_in_game: bool,  // enabled && "file/<file_name>" в level.dat Data.DataPacks.Disabled
      pub description: Option<String>,
      pub worldgen: bool,
      pub needs_restart: bool,     // worldgen ⇒ needs_restart
      pub sha1: String,            // только zip; "" для папок и нечитаемых
  }
  impl DatapackInfo { pub fn id(&self) -> String /* "file/<file_name>" */ }

  pub fn level_dir(server_dir: &Path) -> PathBuf;               // server_dir.join(current_level_name)
  pub fn scan(level_dir: &Path) -> Vec<DatapackInfo>;           // обе папки, сортировка по name без учёта регистра
  pub fn set_enabled(d: &DatapackInfo, enabled: bool, level_dir: &Path) -> Result<PathBuf>;
  ```
- Правила скана: пропускать имена с `.`; файл — только `*.zip` (регистр не важен); папка — только с `pack.mcmeta`.
  `pack.mcmeta` читать не больше 1 MiB. Описание — `pack.description`: строка, объект `{text, extra[]}`
  или массив таких — склеить плоский текст (`fn description_text(v: &serde_json::Value) -> Option<String>`).
- Признаки (`fn content_flags<'a>(paths: impl Iterator<Item = &'a str>) -> (bool, bool)`): для каждого пути
  (имя записи zip или относительный путь в папке, с `/`) найти компонент `data` на позиции 0 или 1
  (позиция 1 — оверлеи), взять папку на два уровня ниже (`data/<ns>/<dir>/…`).
  `WORLDGEN = ["worldgen", "dimension", "dimension_type"]`.
  `RELOADABLE = ["function", "functions", "tags", "recipe", "recipes", "loot_table", "loot_tables",
  "advancement", "advancements", "predicate", "predicates", "item_modifier", "item_modifiers",
  "structure", "structures"]`. `worldgen` — есть dir из WORLDGEN; `needs_restart` — есть dir не из RELOADABLE.
  Нечитаемый zip → `needs_restart = true`, `worldgen = false`.
- `set_enabled` переносит между `datapacks/` и `datapacks.disabled/` (создаёт целевую папку); если цель
  существует — ошибка `файл <имя> уже есть в <папка>` / `file <name> already exists in <folder>`; ошибка
  `rename` — текст из Global Constraints.

- [ ] **Step 1: Тесты** (все в `datapacks::tests`):
  - `scan_lists_zips_and_folders`: в `datapacks/` — `a.zip` (с `pack.mcmeta` `{"pack":{"description":"Hi"}}`),
    папка `B` с `pack.mcmeta`, папка `nopack/` без него, `readme.txt`, `.hidden.zip`; в `datapacks.disabled/` —
    `c.zip` и `.pechka-old/old.zip`. Ожидаем имена `["a", "B", "c"]`, `enabled = [true, true, false]`,
    `description` у `a` == `Some("Hi")`, `is_zip` у `B` == false.
  - `scan_missing_world_is_empty`: `scan(&tmp.join("world"))` == `[]`.
  - `content_flags_by_folders`: `["data/ns/function/a.mcfunction"]` → `(false, false)`;
    `["data/ns/worldgen/biome/x.json"]` → `(true, true)`; `["data/ns/enchantment/x.json"]` → `(false, true)`;
    `["ov_1/data/ns/dimension_type/x.json"]` → `(true, true)`; `["assets/x.png", "pack.mcmeta"]` → `(false, false)`.
  - `description_flattens_components`: `{"text":"A","extra":[{"text":"B"},"C"]}` → `"ABC"`;
    `[{"text":"X"},"Y"]` → `"XY"`; `42` → `None`.
  - `scan_survives_broken_packs`: `bad.zip` с мусором → в списке, `description == None`, `needs_restart == true`;
    zip с `pack.mcmeta` = `{not json` → в списке, `description == None`.
  - `disabled_in_game_from_level_dat`: `level.dat` с `Data.DataPacks.Disabled = ["file/a.zip", "trade_rebalance"]`
    → у `a` `disabled_in_game == true`, у `B` false.
  - `set_enabled_moves_both_ways`: выключить `a` → файл в `datapacks.disabled/a.zip`, `datapacks/a.zip` нет;
    включить обратно → наоборот; папка `B` переносится целиком.
  - `set_enabled_refuses_to_overwrite`: `a.zip` в обеих папках → `set_enabled(a, false)` = `Err`, оба файла на месте и не изменены.
- [ ] **Step 2:** `cargo test datapacks::` → FAIL (нет модуля).
- [ ] **Step 3:** реализовать по Interfaces.
- [ ] **Step 4:** `cargo test datapacks::` → PASS.
- [ ] **Step 5: Commit** `feat: datapacks — скан, перенос, признаки содержимого`.

---

### Task 3: Обновления датапаков через Modrinth

**Files:**
- Modify: `src/plugins.rs` (вынести ядро `check_updates_with`)
- Modify: `src/datapacks.rs`

**Interfaces:**
- Consumes: Task 2 (`DatapackInfo`, `DISABLED_DIR`, `DATAPACK_LOADERS`).
- Produces:
  ```rust
  // plugins.rs
  #[derive(Debug, Default, Clone)]
  pub struct HashCheck {
      pub updates: HashMap<PathBuf, UpdateInfo>,
      pub current: HashMap<PathBuf, String>, // version_number установленного файла, если Modrinth его знает
  }
  pub(crate) type PostFn<'a> = &'a dyn Fn(&str, &serde_json::Value) -> Result<String>;
  pub(crate) fn check_hashes_with(post: PostFn, files: &[(PathBuf, String, Vec<String>)], game_version: Option<&str>) -> Result<HashCheck>;
  // check_updates_with остаётся с той же сигнатурой: строит files из PluginInfo, зовёт check_hashes_with(&post_json, …), отдаёт .updates

  // datapacks.rs
  pub fn check_updates(items: &[DatapackInfo], game_version: Option<&str>) -> Result<HashCheck>; // только zip с sha1, loaders DATAPACK_LOADERS
  pub fn apply_update(d: &DatapackInfo, u: &UpdateInfo, level_dir: &Path) -> Result<PathBuf>;
  fn apply_update_with(d: &DatapackInfo, u: &UpdateInfo, level_dir: &Path, fetch: impl Fn(&str, &Path) -> Result<()>) -> Result<PathBuf>;
  ```
- `apply_update_with`: **имя файла не меняется** (`d.file_name`); загрузка во временный файл в `DISABLED_DIR`;
  проверка sha1 (если есть в `UpdateInfo`) и что это zip; старый файл → `DISABLED_DIR/.pechka-old/<file_name>`,
  а если занято — `<file_name>.1`, `.2`, … (не затирать); новый — на место старого (`d.path`). При ошибке
  после переноса старого — вернуть старый на место. Только для `is_zip`.

- [ ] **Step 1: Тесты:**
  - `plugins::tests::check_hashes_reports_current_and_updates`: подменный `post` отдаёт на `/version_files/update`
    `{"<h1>": {id:"v2", project_id:"P", version_number:"2.0", date_published:"2026-02-01T00:00:00Z", files:[{url:"https://x/n.zip", filename:"n.zip", primary:true, hashes:{sha1:"<h2>"}}]}}`,
    на `/version_files` — `{"<h1>": {id:"v1", …, version_number:"1.0", date_published:"2026-01-01T00:00:00Z", …}}`.
    Ожидаем `updates[path].version == "2.0"`, `current[path] == "1.0"`. Второй файл с неизвестным хэшем — нет ни там, ни там.
    Проверить, что тело запроса содержит `"loaders":["datapack"]`.
  - `datapacks::tests::apply_update_keeps_name_and_backup`: `a.zip` в `datapacks/`, `fetch` пишет новый zip;
    после — `datapacks/a.zip` содержит новые байты, `datapacks.disabled/.pechka-old/a.zip` — старые; повторное
    обновление кладёт прежний в `.pechka-old/a.zip.1`.
  - `datapacks::tests::apply_update_rejects_bad_checksum`: `UpdateInfo.sha1` не совпадает → `Err`, `datapacks/a.zip` прежний,
    во временной папке не осталось `.part`.
- [ ] **Step 2:** `cargo test check_hashes_reports apply_update_` → FAIL.
- [ ] **Step 3:** реализовать; существующие тесты `plugins::` не трогать.
- [ ] **Step 4:** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: обновления датапаков через Modrinth`.

---

### Task 4: Поиск и установка датапаков (`Platform::Datapack`)

**Files:**
- Modify: `src/plugins.rs` (`PluginKind::Datapack`, ветка в `PluginInfo::loaders`)
- Modify: `src/plugin_search.rs`, `src/compat.rs` (только если компилятор потребует ветки)

**Interfaces:**
- Consumes: Task 2 (`scan`, `DatapackInfo`, `DISABLED_DIR`, `DATAPACK_LOADERS`).
- Produces:
  - `PluginKind::Datapack`; `Platform::Datapack` (`label() == "Minecraft"`, `kind() == Datapack`,
    `loaders() == DATAPACK_LOADERS`, `hangar() == None`; `Platform::detect` его никогда не возвращает).
  - `folder(PluginKind::Datapack) == "datapacks"`. Для датапаков параметр `server_dir` у `resolve`, `install`,
    `InstalledIndex::load` — это **папка мира** (`datapacks::level_dir(server)`); док-комментарии обновить.
  - Фасеты Modrinth для `Datapack`: `project_type:datapack`, `categories:datapack`, `versions:<gv>`,
    `server_side!=unsupported`.
  - Файл версии: для `Datapack` — основной файл `*.zip` (вместо `*.jar`); имя проверяется на простое `*.zip`
    без путей.
  - `install_with` для `Datapack`: конфликт — файл с тем же именем в `datapacks.disabled/` (вместо `.jar.disabled`):
    `<имя> уже лежит выключенным (datapacks.disabled)`; сообщение проверки — «не zip-архив»; `note` —
    `Датапак подхватится после перезагрузки датапаков или перезапуска.` / `The datapack loads after a datapack reload or a restart.`
  - `InstalledIndex::load` для `Datapack`: имена/хэши из `datapacks::scan`.

- [ ] **Step 1: Тесты** в `plugin_search::tests`:
  - `datapack_facets_and_no_hangar`: `modrinth_search_url("", &Target::new(Platform::Datapack, Some("26.3")), 0)`
    содержит (после декодирования) `project_type:datapack`, `categories:datapack`, `versions:26.3`;
    `search_with` с подменным `Http` делает ровно один запрос (только Modrinth).
  - `datapack_resolve_picks_zip`: версии проекта — одна с `filename: "x.jar"`, другая с `"x-1.0.zip"`, обе `loaders:["datapack"]`
    → `plan.main.file_name == "x-1.0.zip"`, `plan.kind == PluginKind::Datapack`; URL запроса версий содержит
    `loaders=["datapack"]` (закодированно).
  - `install_datapack_creates_world_dirs`: `install_with(&plan, &tmp.join("world"), fetch)` при отсутствующем мире →
    файл в `world/datapacks/x-1.0.zip`.
  - `install_datapack_refuses_disabled_twin`: `world/datapacks.disabled/x-1.0.zip` существует → `Err`, ничего не скачано.
- [ ] **Step 2:** `cargo test datapack_` → FAIL.
- [ ] **Step 3:** реализовать; ветки `match` по `PluginKind`/`Platform`, которые потребует компилятор, — по смыслу.
- [ ] **Step 4:** `cargo test` → PASS (старые тесты поиска плагинов зелёные).
- [ ] **Step 5: Commit** `feat: поиск и установка датапаков с Modrinth`.

---

### Task 5: Вкладка «Дополнения»: подвкладки, уровни, список датапаков

**Files:**
- Modify: `src/app.rs` (`Tab::Plugins` → `Tab::Addons` через `rename_symbol`, заголовок; новые типы; поле `datapacks`)
- Create: `src/app/addons_tab.rs` (навигация), `src/app/datapacks_tab.rs` (`refresh_datapacks`)
- Modify: `src/app/keys.rs`, `src/app/help.rs`, `src/ui/main_screen.rs`, `src/ui/tests.rs`, `src/ui/snapshots/*` (пересоздать — меняется название вкладки)

**Interfaces:**
- Consumes: Task 1 (`SubLevel`, `sub_arrow`, `sub_tabs_line`), Task 2 (`datapacks::{scan, level_dir, DatapackInfo}`).
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
  pub enum AddonsSub { #[default] Plugins, Datapacks }
  pub struct AddonsNav { pub sub: AddonsSub, pub level: SubLevel }        // App.addons
  pub struct DatapacksView {                                              // App.datapacks
      pub items: Vec<DatapackInfo>, pub sel: usize,
      pub updates: HashMap<PathBuf, UpdateInfo>, pub versions: HashMap<PathBuf, String>,
      pub checking: bool, pub checked: bool, pub busy: Option<String>,
      loaded_for: Option<(usize, PathBuf)>,                               // (id сервера, папка мира)
  }
  pub(super) fn addons_arrow(&mut self, code: KeyCode) -> bool;          // addons_tab.rs, Cycle → cycle_addons_sub
  pub(super) fn cycle_addons_sub(&mut self, d: isize);                     // sel подвкладки не сбрасывается; Entries при пустом списке → Lists
  pub(super) fn refresh_datapacks(&mut self, force: bool);                // datapacks_tab.rs, по образцу refresh_plugins
  fn draw_addons(app: &App, f: &mut Frame, area: Rect, focused: bool);    // вместо draw_plugins
  ```
- Сброс `addons.level = Tabs` и `addons.sub = Plugins` — в `set_tab` и `on_select_changed` (как у «Доступа»).
  `on_tab_shown(Tab::Addons)` обновляет и плагины, и датапаки.
- `move_selection` для `Tab::Addons` — по текущей подвкладке; непустой список → уровень Entries.
- Клавиши `[`/`]` — `cycle_addons_sub` с любого уровня. Пробел и Enter на подвкладке «Плагины» работают
  только на уровне записей; выше — переход в записи и тост `Выберите запись ↑↓ и нажмите <клавиша>` /
  `Pick an entry with ↑↓ and press <key>` (`d` на «Доступе» — тот же приём, текст обобщить).
- Отрисовка: блок «Дополнения»; первая строка — `sub_tabs_line`; ниже — прежнее тело плагинов либо список
  датапаков: колонки имя (24) · версия (`versions`/`—`) · состояние (`вкл` / `выкл` / `выкл в игре`) · пометка
  (`мир` если worldgen, иначе `перезапуск` если needs_restart) · `→ <версия>` при обновлении; строка над списком —
  описание выбранного; пустой список — `Датапаков нет · i — найти и установить` / `No datapacks · i to find and install`.
  Выделение строки — только на уровне Entries.
- Нижняя строка подсказок и справка `?` для `Tab::Addons` — по уровням, как у «Доступа»
  (Tabs: `←→ вкладки · ↓ к спискам · …`; Lists: `←→ плагины/датапаки · ↓ к записям · ↑ к вкладкам · …`;
  Entries: `↑↓ выбор · ←→ плагины/датапаки · …`).

- [ ] **Step 1: Тесты** в `ui::tests`:
  - `addons_tab_levels_and_subtabs`: `4` → `Tab::Addons`, `addons.level == Tabs`; → → `Tab::Settings`; ← → `Tab::Addons`;
    ↓ → Lists; → → `addons.sub == Datapacks`, вкладка та же; ← → Plugins; `]` → Datapacks; ↑ → Tabs; `4` → снова Plugins/Tabs.
  - `addons_on_rcon_server_arrows_switch_tabs`: выбран `rcon_unreachable()`, `4`, ↓ → уровень Tabs не меняется, → → `Tab::Settings`.
  - `addons_datapacks_snapshot`: `local_running()`, `4`, ↓, →, ↓; в `a.datapacks.items` — две записи
    (`Terralith`: zip, enabled, worldgen; `VeinMiner`: zip, выключен, описание) и `versions` для первой `"2.6.5+26.3"`;
    снимок `addons_datapacks_en` 120×34.
- [ ] **Step 2:** `cargo test addons_` → FAIL.
- [ ] **Step 3:** реализовать; `PECHKA_UPDATE_SNAPSHOTS=1 cargo test ui::tests`, просмотреть `git diff src/ui/snapshots`
  (меняется только «Плагины» → «Дополнения» в строке вкладок + новый снимок).
- [ ] **Step 4:** `cargo test` → PASS.
- [ ] **Step 5: Commit** `feat: вкладка «Дополнения» с подвкладками плагинов и датапаков`.

---

### Task 6: Действия с датапаками: вкл/выкл, перезагрузка, обновления, поиск

**Files:**
- Modify: `src/app.rs` (`Action`), `src/app/datapacks_tab.rs`, `src/app/addons_tab.rs`, `src/app/keys.rs`, `src/app/panel.rs`,
  `src/app/help.rs`, `src/ui/main_screen.rs`, `src/ui/tests.rs`, `README.md`, `CHANGELOG.md`

**Interfaces:**
- Consumes: Task 2 (`set_enabled`), Task 3 (`check_updates`, `apply_update`, `HashCheck`), Task 4 (`Platform::Datapack`),
  Task 5 (`DatapacksView`, `AddonsSub`), `players_note`, `Request::Command`.
- Produces:
  ```rust
  // Action
  ReloadDatapacks(usize),                 // id сервера → Command "minecraft:reload"
  DatapackToggle(PathBuf),                // после подтверждения для worldgen
  ApplyDatapackUpdates(Vec<PathBuf>),
  // datapacks_tab.rs
  pub(super) fn toggle_datapack(&mut self);
  pub(super) fn after_datapack_change(&mut self, needs_restart: bool, what: String);
  pub(super) fn check_datapack_updates(&mut self);
  pub(super) fn ask_apply_datapack_updates(&mut self, all: bool);
  pub(super) fn apply_datapack_updates(&mut self, paths: Vec<PathBuf>);
  // panel.rs: open_plugin_search(&mut self) → open_addon_search(&mut self, sub: AddonsSub)
  ```
- `toggle_datapack`: включённый, но `disabled_in_game` → на запущенном `datapack enable "file/<имя>"` + тост; на остановленном —
  тост `Он выключен в игре командой /datapack — включить можно на запущенном сервере`. Выключение `worldgen` →
  `Popup::Confirm` (`<имя> меняет генерацию мира: уже созданные места могут сломаться. Выключить?`) с `DatapackToggle`.
  Иначе — `set_enabled`, `refresh_datapacks(true)`, `after_datapack_change(d.needs_restart, "<имя>: выключен|включён")`.
- `after_datapack_change`: сервер не запущен → тост `what`; `needs_restart` → тост `what + " — вступит в силу после перезапуска"`;
  есть игроки → `Popup::Confirm { title: "Перезагрузить датапаки", text: players_note + " Перезагрузка даёт короткую заминку.", action: ReloadDatapacks(id) }`;
  иначе — `minecraft:reload` и тост `what + " · датапаки перезагружаются"`.
- Клавиши подвкладки «Датапаки»: `i`/`n` поиск, Пробел вкл/выкл, `c` проверка, `Enter` обновить выбранный, `U` все,
  `u` — `refresh_datapacks(true)`. Пробел и `Enter` — только на уровне записей, выше — тот же переход и тост, что в Task 5.
- Обновления: `c` сохраняет `HashCheck.updates` в `datapacks.updates`, а `HashCheck.current` — в `datapacks.versions`
  (колонка версии). `Enter`/`U` — как у плагинов; подтверждение перечисляет `имя → версия`,
  `Старые файлы уйдут в datapacks.disabled/.pechka-old/`, и для worldgen-паков строку
  `⚠ <имена> меняют генерацию мира: изменения коснутся только новых чанков.`; после — `after_datapack_change(any needs_restart, "обновлено: N")`.
- Поиск: `i` на «Датапаках» → `Target::new(Platform::Datapack, s.version)` без определения ядра (работает и на ванильном
  сервере), заголовок ` Поиск датапаков · Minecraft <ver> `,
  строка источников `Источник: Modrinth.`; `pick_plugin`/`install_plugin` берут базовую папку
  `datapacks::level_dir(server)` для `Platform::Datapack`; в подтверждении установки датапака при существующем мире —
  `⚠ Если датапак меняет генерацию мира, изменения коснутся только новых чанков.`; после установки —
  `refresh_datapacks(true)` и `after_datapack_change(<needs_restart установленного файла по свежему скану>, …)`.
- README: строка «Плагины» в списке вкладок и таблице клавиш → «Дополнения» с датапаками; CHANGELOG `[Unreleased]` → «Добавлено».

- [ ] **Step 1: Тесты** в `ui::tests` (временная папка сервера с `server.properties` `level-name=world` и zip-датапаками,
  `ServerConfig::new_local` на неё; статус Running):
  - `datapack_toggle_with_players_asks_before_reload`: пак только с `data/ns/function/…`, игроки `["Alex"]`; `4`,↓,→,↓, Пробел →
    файл в `world/datapacks.disabled/`, `popup` = `Confirm` с `Action::ReloadDatapacks(_)`.
  - `datapack_toggle_without_players_reloads`: игроков нет → `popup.is_none()`, тост содержит `datapacks are reloading` (En).
  - `worldgen_datapack_asks_then_needs_restart`: пак с `data/ns/worldgen/biome/x.json` → Пробел даёт `Confirm` с `DatapackToggle`;
    подтверждение → файл перенесён, тост содержит `after a restart`, `ReloadDatapacks` не предлагается.
  - `datapack_disabled_in_game_on_stopped_server`: `level.dat` с `Disabled=["file/a.zip"]`, сервер остановлен → Пробел
    не переносит файл, тост содержит `/datapack`.
- [ ] **Step 2:** `cargo test datapack_ worldgen_datapack` → FAIL.
- [ ] **Step 3:** реализовать; обновить справку, подсказки, README, CHANGELOG.
- [ ] **Step 4:** `cargo test`, `cargo fmt --check`, `cargo clippy --all-targets` → чисто.
- [ ] **Step 5: Commit** `feat: включение, обновление и установка датапаков`.

---

### Task 7: Проверка вживую на песочнице

**Files:** только scratchpad (копия сервера на других портах); код не меняется, если проверка не нашла расхождений.

- [ ] **Step 1:** Собрать `cargo build`. Сделать изолированную копию сервера в scratchpad (без `world/`, свои
  `server-port`/`rcon.port`/`query.port`), конфиг pechka в scratchpad, переменные
  `PECHKA_NO_SERVICE=1 PECHKA_NO_UPDATE_CHECK=1 PECHKA_NO_NOTIFY=1 PECHKA_LANG=ru`. Два тестовых пака:
  `fn.zip` (`data/t/function/hi.mcfunction` = `say hi`) и `wg.zip` (`data/t/worldgen/biome/…`).
- [ ] **Step 2:** Через pty-драйвер (pyte из venv в scratchpad): запустить сервер, `4`,↓,→ — оба пака видны, у `wg` пометка `мир`.
  Выключить `fn` → в логе сервера перезагрузка, `datapack list` в консоли не содержит `file/fn.zip`; включить → содержит.
  Выключить `wg` → подтверждение, затем тост «после перезапуска», `minecraft:reload` не отправлен.
- [ ] **Step 3:** Если поведение `minecraft:reload` расходится со спекой (новый пак не включается, пропавший не
  выключается) — записать наблюдение в отчёт и выбрать вариант ближе к «как у плагинов», не трогая мир.
- [ ] **Step 4:** Остановить песочницу (`pechka --config <file> daemon stop`), убедиться `pgrep -f <sandbox>` пуст.
  Записать проверенные факты про `minecraft:reload` в память Serena (`gotchas`).
