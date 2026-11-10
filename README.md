# folder-tidy

把 Mac 上亂掉的資料夾（桌面、下載、iCloud Drive）依規則整理進分類資料夾：先預覽，確認後才搬，搬錯可以復原。

Organize a messy Mac folder into category folders by rules. You always see
the plan first, nothing moves until you confirm, and every move can be
undone.

- **Rules**: by file extension, by keyword in the file name, and by modified
  date (fixed dates or "within the last N days"). Folders can nest
  (`工作/專案A`). First matching rule wins.
- **Preview**: files grouped by destination, with Quick Look thumbnails;
  change one file's destination, select or move a whole group, or clear the
  preview.
- **Safe moves**: a move never overwrites (atomic `renamex_np` with
  `RENAME_EXCL`); a file whose destination already exists is skipped and
  reported. Every apply is written to a move log, and **undo** puts the files
  back.
- **AI suggestions (optional)**: files no rule matches can be sent to your
  local [Claude Code](https://claude.com/claude-code) CLI. Only the file name,
  size and date are sent, never the contents, and folders or extensions listed
  in `ai_exclude` are never sent at all.
- **Local only**: the web UI binds `127.0.0.1` and refuses other hosts.

The UI is in Traditional Chinese.

## Requirements

- macOS (uses `renamex_np`, Quick Look's `qlmanage` and `osascript`)
- Rust 1.85+ (edition 2024)
- Optional: the `claude` CLI, logged in, for AI suggestions

## Run

```sh
cargo build --release

# A rules file; the UI saves its settings back to it.
cat > rules.toml <<'TOML'
ai_exclude = ["~/Documents/tax", "*.key"]

[[category]]
name = "截圖"
keywords = ["Screenshot"]

[[category]]
name = "圖片"
extensions = ["jpg", "png", "heic"]

[[category]]
name = "工作/報表"
keywords = ["月報表"]
TOML

./target/release/folder-tidy serve --config rules.toml --log moves.json
# open http://127.0.0.1:8737/
```

Pick a folder on **來源**, review **預覽**, press **確認搬移**. **紀錄** lists
past applies and undoes the latest one.

### Command line

```sh
folder-tidy scan  ~/Desktop --config rules.toml            # categories as JSON
folder-tidy plan  ~/Desktop --config rules.toml            # dry run
folder-tidy apply ~/Desktop --config rules.toml --log moves.json --yes
folder-tidy undo  --log moves.json
```

Add `--recursive` to include subfolders.

## Test

```sh
cargo test
```

Tests run only on temporary folders and fixtures. They never touch your
files, never call the real `claude` CLI and never open the folder picker.

## License

MIT
