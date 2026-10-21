# NavDB Studio – Architecture

NavDB Studio is a native multi-database GUI client written from scratch in Rust.

## Workspace

```
crates/navdb-core   GUI-free library (unit tested)
crates/navdb-app    egui/eframe desktop application (binary `navdb`)
```

### navdb-core

| module | purpose |
| --- | --- |
| `config` | `ConnectionConfig`, `DbKind`, `Preferences`, `SavedQuery`, `Snippet`, `AutomationJob`, `Workspace` (persisted JSON), history log |
| `value` | `Value` cell enum, `ColumnMeta`, `QueryResult` |
| `model` | `ObjPath`, `Scope`, `ObjectKind`, `ObjectInfo`, `TableDef`/`ColumnDef`/`IndexDef`/`ForeignKeyDef`/`TriggerDef`/`CheckDef`, `UserInfo`, `UserSpec`, `ServerSnapshot` |
| `dialect` | quoting, literals, qualified names, type lists per SQL dialect |
| `driver` | `Driver` trait + MySQL/MariaDB, PostgreSQL, SQLite, Redis drivers; `driver::connect`, `driver::test_connection` |
| `session` | `Session` – a worker thread owning one driver connection. `session.run(|d: &mut dyn Driver| -> Result<T>)` returns `Pending<T>`; poll it each frame with `.poll()`. `session.fork()` opens an independent connection. `session::spawn_task(notify, f)` runs a closure on a fresh thread. |
| `ddl` | CREATE/ALTER/DROP generation (`create_table`, `alter_table`, `alter_table_with_renames`, `drop_object`, `rename_object`, `create_view`, `create_database`, `duplicate_table`, `script`) |
| `dml` | SELECT paging, filters (`FilterRule`), INSERT/UPDATE/DELETE/UPSERT builders |
| `sqlutil` | statement splitter (quotes, comments, `DELIMITER`, `$$`), beautify/minify, keywords |
| `export` | `ExportFormat` (CSV, TSV, TXT, JSON, XML, HTML, Markdown, SQL, Excel-XML), `export_to_file` |
| `import` | `ImportFormat` (CSV/TSV/JSON/XML) parsing, `FieldMapping`, `ImportMode`, `infer_table`, `build_statements` |
| `dump` | `dump::dump(driver, scope, &DumpOptions, writer, progress)` – SQL dump of a database |
| `datagen` | `Generator` per column (`Generator::guess`, `generate_rows`) |
| `sync` | `compare_structures`/`structure_script`, `compare_data`/`data_script` |
| `ssh` | SSH tunnel via system `ssh -L` |

All drivers are synchronous; the GUI must **never** call a driver directly –
always through `Session::run`, then poll the `Pending`.

### navdb-app

```
main.rs        eframe entry
app.rs         main window: menu bar, toolbar, navigator, tab bar, info pane, status bar, toasts
state.rs       AppState (workspace, open connections, selection, actions, toasts, background tasks)
commands.rs    every user command (open/design/drop/rename objects, open tools...) – menus call these
ui/            shared widgets: grid (data grid + form view), sql_editor, tree (navigator), info_pane, style (icons/colors), mod.rs helpers (scope_picker, kv_grid, file dialogs, loading/error labels)
tabs/          main-area tabs implementing `tabs::Tab`
dialogs/       floating windows implementing `dialogs::Dialog`
```

Key APIs for tab/dialog authors:

* `st.session(conn_id) -> Option<Session>` – shared connection of the navigator (opens it if needed).
  Long-running tools should call `.fork()` to get their own connection.
* `st.config(conn_id)` – `ConnectionConfig` (`.dialect()`, `.kind`).
* `st.toast(msg)`, `st.error(msg)`, `st.open_tab(tab)`, `st.open_dialog(dialog)`,
  `st.refresh_scope(conn, &scope)`, `st.run_task(conn, scope, sql, success_msg, refresh)`,
  `st.record(conn, scope, sql, ok, message, elapsed_ms)` (History Log), `st.save_workspace()`.
* `st.ws` – persisted `Workspace` (connections, preferences, queries, snippets, automation jobs).
* `crate::state::Loadable<T>` – NotLoaded / Loading(Pending) / Ready / Failed, with `.poll()`.
* `crate::ui::grid::data_grid(ui, id, &source, &mut GridState, &GridOptions) -> Vec<GridEvent>`
  works for any `QueryResult` (implements `GridSource`).
* `crate::ui::sql_editor::SqlEditor` – `editor.ui(ui, id, &extra_completion_words)`.
* `crate::ui::scope_picker(ui, id, st, &mut Option<conn>, &mut Option<Scope>)`.
* `crate::dialogs::window(title, id)` – standard window builder; a dialog returns
  `false` from `show` to close.
* Tab `key()` must be unique per logical object (it is used to focus an existing tab instead of opening a duplicate).
* Tabs must poll their `Pending`s in `ui()` (and optionally in `tick()` when hidden).

egui version: **0.31** (egui_extras 0.31 with syntect, egui_plot 0.31).
Use `ui.close_menu()`, `egui::Frame::new()`, `corner_radius`, `StrokeKind`,
`ComboBox::from_id_salt`.
