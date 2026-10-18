# rvpacker-txt-rs

[README на русском](./README-ru.md)

A command-line tool that extracts translatable text from RPG Maker 2000/2003/XP/VX/VX Ace/MV/MZ game files into plain `.txt` files, and writes translated `.txt` files back into the game's original format. It also decrypts `.rgss` archives when it finds one.

It's built on top of [rvpacker-txt-rs-lib](https://github.com/RPG-Maker-Translation-Tools/rvpacker-txt-rs-lib). Use that crate directly if you're building your own tool.

## Related tools

- [rpgmtranslate-qt](https://github.com/RPG-Maker-Translation-Tools/rpgmtranslate-qt) - a GUI for editing the translation files this tool produces.
- [rpgm-asset-decrypter-rs](https://github.com/rpg-maker-translation-tools/rpgm-asset-decrypter-rs) and [rpgm-archive-decrypter](https://github.com/RPG-Maker-Translation-Tools/rpgm-archive-decrypter) - CLIs for decrypting/encrypting assets and `.rgss` archives on their own.
- [rpgmdec](https://github.com/RPG-Maker-Translation-Tools/rpgmdec) - a GUI combining both decrypters.

The name comes from the original `rvpacker`, whose repository no longer exists. The Ruby tool this was renamed from lives on at [rvpacker-txt](https://github.com/savannstm/rvpacker-txt).

## Installation

Get the binaries from the **Releases** section. They require an **[x86-64-v2](https://en.wikipedia.org/wiki/X86-64#Microarchitecture_levels)** CPU with AES-NI.

## How it works

### Directory layout

Point `--input-dir` (`-i`, defaults to `./`) at the folder that holds the game's data. The engine is detected automatically.

- XP/VX/VX Ace: the game folder, with a `data`/`Data` directory and the `.rgss` archive if there is one (`Game.rgssad`, `Game.rgss2a` or `Game.rgss3a`).
- MV/MZ: the `www` folder, which holds the `data`/`Data` directory with `System.json`.
- RPG Maker 2000/2003: `RPG_RT.ldb`, `RPG_RT.lmt` and `MapNNNN.lmu` files sit directly in the input directory. There's no `Scripts`/plugin equivalent and no `.rgss` archive.
- Archive-only games (no `data`/`Data` directory): `read` and `json generate` extract the archive first. Names are read as UTF-8, or as `--archive-read-encoding` if given, else `--read-encoding`. Entries whose names don't decode are skipped with a warning, so older Japanese archives need `--archive-read-encoding shift-jis`.

`read` writes `.txt` files into `<output-dir>/translation`, and `write` writes the game's files into `<output-dir>/output`. `--output-dir` (`-o`) defaults to the input directory.

### Text encoding

Reading the game's text and writing a translation back are independent, controlled by `--read-encoding` and `--write-encoding`.

**`--read-encoding`.** RPG Maker 2000/2003 files and XP/VX's `Scripts.*` carry no encoding. If unset, text is read as UTF-8, and text that isn't valid UTF-8 fails the run with an error. Nothing is guessed. Pass the codepage the game was saved in, such as `shift-jis`. VX Ace tags most strings with their encoding, so this rarely matters there.

To find the codepage, use `--probe-ini-title <ENCODING>` on its own to decode the `Game.ini` title (or `RPG_RT.ini`'s) with each candidate. The title is the one field in those files that can carry non-ASCII bytes, and it uses the same codepage as the rest of the game's text. Once you've found it, pass `--read-encoding` with `--use-ini-title` to write the decoded title into the system file.

**`--write-encoding` is unset by default, which writes UTF-8. Leave it unset unless you need otherwise.** Forcing a codepage that can't represent the translation doesn't fail loudly: unmappable characters are silently replaced with `&#1055;`-style numeric references. Only set it when the target engine has no Unicode text rendering (RPG Maker 2000/2003, XP, VX), and the translation fits that codepage. The player's system then needs the same codepage, or a locale emulator. On Windows 10 1903+, enabling "Beta: Use Unicode UTF-8 for worldwide language support" under Region settings makes UTF-8 work for these engines too. VX Ace is Unicode-native and needs none of this.

Both flags take a WHATWG encoding label, such as `utf-8`, `shift-jis`, `gb18030`, `euc-kr` (UHC/CP949), `big5` or `windows-1251`. Non-ASCII-compatible encodings such as UTF-16 are rejected. `--read-encoding` is recorded in `.rvpacker-metadata`, so later runs reuse it. `--write-encoding` is never recorded, so pass it on every `write`.

### Translation file format

Each `.txt` line is a source text, a `<#>` separator, then the translation. Line breaks in the source are written as `\#`, and translations should use `\#` the same way.

```txt
<!>ID<#>2
<!>NAME<#>City
<!>ORDER<#>157
<!>IN-GAME DISPLAYED NAME: City<#>Translated City
This is sample text<#>This is translated sample text
This is sample\#multiline text<#>This is translated sample\#multiline text
```

`<#>`, `\#` and `<!>` (the comment prefix) can be changed per project with `--line-separator`, `--line-break` and `--comment-prefix`. The full format, including comments and duplicate handling, is in the [library's README](https://github.com/RPG-Maker-Translation-Tools/rvpacker-txt-rs-lib#translation-file-format).

### `.rvpacker-metadata` and `.rvpacker-ignore`

Each `read` writes `.rvpacker-metadata` into the `translation` directory. It records the duplicate mode, format settings, `--map-events`, and content hashes. Later runs load it automatically.

Settings resolve in this order: a flag passed on the command line, then the recorded value, then the default (`--duplicate-mode` defaults to `allow`). A passed flag applies to this run and, for `read`, to the metadata it writes. `--map-events` can only be switched on this way. To turn it off, delete `.rvpacker-metadata` and run `read` again. Only `append` uses the recorded hashes, to skip files that haven't changed. A run that changes a text setting (separator, break, prefix, read encoding, or turning on `--map-events`) ignores them.

`.rvpacker-ignore` excludes specific lines from extraction, for unused or duplicate content. Pass `--ignore` to `read` to apply it, or `--create-ignore` to `purge` to generate one from purged lines. See the [library's README](https://github.com/RPG-Maker-Translation-Tools/rvpacker-txt-rs-lib#rvpacker-ignore) for the syntax, and [`examples/.rvpacker-ignore`](https://github.com/RPG-Maker-Translation-Tools/rvpacker-txt-rs-lib/blob/main/examples/.rvpacker-ignore) for an example.

## Usage

Run `rvpacker-txt-rs -h` for general help, or `rvpacker-txt-rs <command> -h` for a command's options.

### `read`

Extracts text from the game's files into `translation/*.txt`.

```bash
rvpacker-txt-rs read -i "C:/Game"
```

`-r, --read-mode <MODE>` (alias `--mode`) controls existing translation files:

- `default` - leaves existing translation files alone and logs each one. Only missing files are written.
- `append` - adds new text and keeps existing translations. Files whose content hasn't changed since the last read are skipped. Translations for text no longer in the game are kept unless `--skip-obsolete` is set.
- `force` - rewrites existing translation files, after asking for confirmation.
- `force-append` - `append` plus `force`, which bypasses the hash check.

`read` also takes `-I, --ignore` (apply `.rvpacker-ignore`), `-m, --map-events` (record where in the map events each line comes from), and `--skip-obsolete` (alias `--so`), which drops the obsolete translations instead of keeping them.

`read`, `write` and `purge` share these options:

- `-s, --skip-files <FILES>` (alias `--skip`) - comma-separated file kinds to skip, matched exactly and ignoring case: `map`, `actors`, `armors`, `classes`, `commonevents`, `enemies`, `items`, `skills`, `states`, `system`, `troops`, `weapons`, `scripts`, `database` (RM2K only). `plugins` and `scripts` are interchangeable.
- `--skip-maps <INDICES>` (alias `--sm`) - map ids to skip, e.g. `0,1,5-8`.
- `--skip-events <SPEC>` (alias `--se`) - per-file event ids to skip, as `file:0,1,..;file:0,1,..`. File names must match exactly, e.g. `commonevents`; an unknown name is an error. Has no effect on maps.
- `-d, --duplicate-mode <allow|remove>` (alias `--dup-mode`).

### `write`

Writes translated `.txt` files back into the game's original format, under `<output-dir>/output`.

```bash
rvpacker-txt-rs write -i "C:/Game"
```

### `purge`

Drops translation entries that were never filled in.

```bash
rvpacker-txt-rs purge -i "C:/Game"
```

### `json`

Converts binary data files to and from JSON, independent of the translation workflow. Covers XP/VX/VX Ace's Marshal files and RPG Maker 2000/2003's `RPG_RT.ldb`/`RPG_RT.lmt`/`MapNNNN.lmu` files. Not for MV/MZ, which already use JSON: `json generate` fails on them.

```bash
rvpacker-txt-rs json generate -i "C:/Game"   # writes C:/Game/json/*.json (and Scripts.rb)
rvpacker-txt-rs json write -i "C:/Game"      # reads C:/Game/json back into C:/Game/json-output
```

`json generate` skips files whose JSON already exists. `-r force` (alias `--mode force`) overwrites them.

For RPG Maker 2000/2003, the original extension is kept before `.json` (`RPG_RT.ldb.json`, `RPG_RT.lmt.json`, `MapNNNN.lmu.json`), because `RPG_RT.ldb` and `RPG_RT.lmt` would otherwise collide.

### `serde`

Exports `translation/*.txt` to a structured format for editing in external tools, and imports it back. Supports JSON, CSV, XML, XLSX and YAML.

```bash
rvpacker-txt-rs serde export json -i "C:/Game"   # writes C:/Game/translation/*.txt to C:/Game/serde/*.json
rvpacker-txt-rs serde import json -i "C:/Game"   # reads C:/Game/serde/*.json back into C:/Game/translation/*.txt
```

`export` writes one file per `.txt` into `<output-dir>/serde`. `import` overwrites the matching `.txt` for every file of the format's extension found there. JSON and YAML store each line as a tagged `comment` or `translation` entry. CSV and XLSX use one flat `type, source, translation_count, translation_1..N` row layout. XML uses `<comment>` and `<entry>` elements. See the library's [`serde` module](https://github.com/RPG-Maker-Translation-Tools/rvpacker-txt-rs-lib/blob/main/src/serde.rs) for the exact schemas.

### `asset`

Decrypts and encrypts MV/MZ `.png`, `.ogg` and `.m4a` assets, without parsing the game. Same as [rpgm-asset-decrypter-rs](https://github.com/rpg-maker-translation-tools/rpgm-asset-decrypter-rs). Uses `-i` and `-o` as directories.

```bash
rvpacker-txt-rs asset decrypt -i "C:/Game/www/img/pictures"   # .rpgmvp/.png_ => .png, next to the input or in -o
rvpacker-txt-rs asset encrypt -i "C:/Out" -E mz -e <KEY>      # .png => .rpgmvp (MZ: .png_)
rvpacker-txt-rs asset extract-key -f "C:/Game/www/data/System.json"
```

`decrypt` deduces the key for each file, unless `-e` is given. `--file` processes a single file instead of the input directory. `--force` makes `decrypt` overwrite existing output; `encrypt` always overwrites. Only files directly inside `-i` are processed, not subfolders.

### `archive`

Extracts and creates RPG Maker XP/VX/VX Ace `.rgss` archives, without parsing the game. Same as [rpgm-archive-decrypter](https://github.com/RPG-Maker-Translation-Tools/rpgm-archive-decrypter). Uses `-i` as the archive or its directory, and `-o` as the output.

```bash
rvpacker-txt-rs archive decrypt -i "C:/Game"                         # extracts the .rgss in C:/Game into C:/Game
rvpacker-txt-rs archive encrypt -i "C:/Game" -e vxace                # writes C:/Game/Game.rgss3a
```

`--encoding` sets the encoding of file names inside the archive, e.g. `shift_jis` for Japanese XP/VX games. Encrypt packs `data` and `graphics` by default; use `-E` and `--additional-encrypt-dirs` to change that. `encrypt` asks before overwriting an existing archive. `decrypt -d` (`--decrypt-files`) extracts only the listed entries, separated by `:`.

### Global options

These apply to every command:

- `-i, --input-dir <PATH>` - defaults to `./`.
- `-o, --output-dir <PATH>` - defaults to the input directory.
- `--line-separator <SEPARATOR>`, `--line-break <BREAK>`, `--comment-prefix <PREFIX>` - override the translation-file format defaults (`<#>`, `\#`, `<!>`). See [`.rvpacker-metadata` and `.rvpacker-ignore`](#rvpacker-metadata-and-rvpacker-ignore).
- `--read-encoding <ENCODING>` and `--write-encoding <ENCODING>` - see [Text encoding](#text-encoding).
- `--archive-read-encoding <ENCODING>` - codepage for the file names in a `.rgss` archive that `read` or `json generate` extracts. Unset, `--read-encoding` is used. Not recorded in `.rvpacker-metadata`, since the archive is extracted only once.
- `-v`/`-q` - increase/decrease log verbosity.

## Building

You need Rust `1.88`.

```bash
git clone https://github.com/RPG-Maker-Translation-Tools/rvpacker-txt-rs
cd rvpacker-txt-rs
cargo build --release
```

The binary is `target/release/rvpacker-txt-rs` (`rvpacker-txt-rs.exe` on Windows).

### Features

| Feature    | Default | Effect                                                                                                    |
| ---------- | ------- | --------------------------------------------------------------------------------------------------------- |
| `gxhash`   | yes     | Hashes maps and sets with gxhash, which needs AES-NI (or Armv8 crypto). Without it, rapidhash is used.    |
| `mimalloc` | yes     | Uses the mimalloc allocator.                                                                              |

Disable the defaults to drop `gxhash` or `mimalloc`:

```bash
cargo build --release --no-default-features                       # rapidhash, system allocator
cargo build --release --no-default-features --features mimalloc   # rapidhash, mimalloc
```

## Support

[Me](https://github.com/savannstm), the maintainer of this project, is a poor college student from Eastern Europe.

If you could, please consider supporting us through:

- [Boosty](https://boosty.to/mcdeimos) (has lower fees)
- [Ko-fi](https://ko-fi.com/savannstm)

Even if you don't, we will still continue working on the project anyways.

## License

Licensed under [WTFPL](https://www.wtfpl.net/).
