# rvpacker-txt-rs

[README in English](./README.md)

Инструмент командной строки, который извлекает переводимый текст из игровых файлов RPG Maker 2000/2003/XP/VX/VX Ace/MV/MZ в обычные `.txt` файлы и записывает переведённые `.txt` файлы обратно в исходный формат игры. Он также расшифровывает `.rgss` архивы, если находит их.

Он построен на основе [rvpacker-txt-rs-lib](https://github.com/RPG-Maker-Translation-Tools/rvpacker-txt-rs-lib). Используйте эту библиотеку напрямую, если создаёте собственный инструмент.

## Связанные инструменты

- [rpgmtranslate-qt](https://github.com/RPG-Maker-Translation-Tools/rpgmtranslate-qt) - графический интерфейс для редактирования файлов перевода, которые создаёт этот инструмент.
- [rpgm-asset-decrypter-rs](https://github.com/rpg-maker-translation-tools/rpgm-asset-decrypter-rs) и [rpgm-archive-decrypter](https://github.com/RPG-Maker-Translation-Tools/rpgm-archive-decrypter) - утилиты командной строки для расшифровки и зашифровки ассетов и `.rgss` архивов отдельно.
- [rpgmdec](https://github.com/RPG-Maker-Translation-Tools/rpgmdec) - графический интерфейс, объединяющий оба декодера.

Название происходит от оригинального `rvpacker`, репозиторий которого больше не существует. Ruby-инструмент, из которого этот был переименован, доступен в [rvpacker-txt](https://github.com/savannstm/rvpacker-txt).

## Установка

Скомпилированные файлы берите в разделе **Releases**. Им нужен процессор с поддержкой **[x86-64-v2](https://en.wikipedia.org/wiki/X86-64#Microarchitecture_levels)** и AES-NI.

## Как это работает

### Расположение папок

Укажите `--input-dir` (`-i`, по умолчанию `./`) на папку с данными игры. Движок определяется автоматически.

- XP/VX/VX Ace: папка игры, с каталогом `data`/`Data` и архивом `.rgss`, если он есть (`Game.rgssad`, `Game.rgss2a` или `Game.rgss3a`).
- MV/MZ: папка `www`, в которой лежит каталог `data`/`Data` с `System.json`.
- RPG Maker 2000/2003: файлы `RPG_RT.ldb`, `RPG_RT.lmt` и `MapNNNN.lmu` лежат прямо во входной директории. Отдельных `Scripts`/плагинов и `.rgss` архива нет.
- Игры только с архивом (нет каталога `data`/`Data`): `read` и `json generate` сначала распаковывают архив. Имена читаются как UTF-8 или в кодировке `--read-encoding`, если она задана. Записи, чьи имена не декодируются, пропускаются с предупреждением, поэтому старым японским архивам нужен `--read-encoding shift-jis`.

`read` записывает `.txt` файлы в `<output-dir>/translation`, а `write` записывает файлы игры в `<output-dir>/output`. `--output-dir` (`-o`) по умолчанию равен входной директории.

### Кодировка текста

Чтение текста игры и запись перевода обратно независимы и управляются флагами `--read-encoding` и `--write-encoding`.

**`--read-encoding`.** Файлы RPG Maker 2000/2003 и `Scripts.*` в XP/VX не хранят кодировку. Если флаг не задан, текст читается как UTF-8, а текст, который не является корректным UTF-8, приводит к ошибке. Ничего не угадывается. Передавайте кодировку, в которой сохранена игра, например `shift-jis`. VX Ace помечает большинство строк их кодировкой, поэтому там это обычно не важно.

Чтобы найти кодировку, используйте `--probe-ini-title <ENCODING>` отдельно: он декодирует название из `Game.ini` (или `RPG_RT.ini`) в указанной кодировке. Название — единственное поле в этих файлах, которое может содержать не-ASCII байты, и оно использует ту же кодировку, что и остальной текст игры. Найдя подходящую кодировку, передайте `--read-encoding` вместе с `--use-ini-title`, чтобы записать декодированное название в системный файл.

**`--write-encoding` по умолчанию не задан, и записывается UTF-8. Не задавайте его, если в этом нет необходимости.** Если кодировка не может представить перевод, ошибки не будет: недопустимые символы молча заменяются числовыми ссылками вида `&#1055;`. Задавайте её, только если движок игры не поддерживает юникод (RPG Maker 2000/2003, XP, VX) и перевод помещается в эту кодировку. Тогда у игрока тоже должна быть эта кодировка или эмулятор локали. В Windows 10 1903+ включение опции «Бета-версия: использовать UTF-8 для поддержки языков во всём мире» в настройках региона позволяет UTF-8 работать и для этих движков. VX Ace поддерживает юникод изначально и ничего из этого не требует.

Оба флага принимают название кодировки WHATWG, например `utf-8`, `shift-jis`, `gb18030`, `euc-kr` (UHC/CP949), `big5` или `windows-1251`. Кодировки, не совместимые с ASCII, например UTF-16, отклоняются. `--read-encoding` записывается в `.rvpacker-metadata`, поэтому последующие запуски используют её. `--write-encoding` не записывается, поэтому передавайте его при каждом `write`.

### Формат файла перевода

Каждая строка `.txt` содержит исходный текст, разделитель `<#>` и перевод. Переносы строк в исходном тексте записываются как `\#`, и в переводе их тоже нужно писать как `\#`.

```txt
<!>ID<#>2
<!>NAME<#>Город
<!>ORDER<#>157
<!>IN-GAME DISPLAYED NAME: Город<#>Переведённый город
Это пример текста<#>Это переведённый пример текста
Это пример многострочного\#текста<#>Это переведённый пример многострочного\#текста
```

`<#>`, `\#` и `<!>` (префикс комментария) можно изменить для проекта через `--line-separator`, `--line-break` и `--comment-prefix`. Полный формат, включая комментарии и обработку дубликатов, описан в [README библиотеки](https://github.com/RPG-Maker-Translation-Tools/rvpacker-txt-rs-lib#translation-file-format).

### `.rvpacker-metadata` и `.rvpacker-ignore`

Каждый `read` записывает `.rvpacker-metadata` в каталог `translation`. В нём хранятся режим дубликатов, настройки формата, `--map-events` и хэши содержимого. Последующие запуски загружают его автоматически.

Настройки определяются в таком порядке: флаг из командной строки, затем сохранённое значение, затем значение по умолчанию (`--duplicate-mode` по умолчанию `allow`). Переданный флаг действует на этот запуск и, для `read`, на записываемые метаданные. `--map-events` можно только включить таким способом; чтобы выключить, удалите `.rvpacker-metadata` и запустите `read` заново. Только `append` использует сохранённые хэши, чтобы пропускать файлы, которые не изменились. Запуск, который меняет настройку текста (разделитель, перенос строки, префикс, кодировку чтения или включение `--map-events`), их игнорирует.

`.rvpacker-ignore` исключает отдельные строки из извлечения, например неиспользуемые или повторяющиеся. Передайте `--ignore` команде `read`, чтобы его применить, или `--create-ignore` команде `purge`, чтобы создать его из удалённых строк. Синтаксис см. в [README библиотеки](https://github.com/RPG-Maker-Translation-Tools/rvpacker-txt-rs-lib#rvpacker-ignore), рабочий пример — в [`examples/.rvpacker-ignore`](https://github.com/RPG-Maker-Translation-Tools/rvpacker-txt-rs-lib/blob/main/examples/.rvpacker-ignore).

## Использование

Запустите `rvpacker-txt-rs -h` для общей справки или `rvpacker-txt-rs <command> -h` для опций команды.

### `read`

Извлекает текст из файлов игры в `translation/*.txt`.

```bash
rvpacker-txt-rs read -i "C:/Game"
```

`-r, --read-mode <MODE>` (алиас `--mode`) определяет, что делать с существующими файлами перевода:

- `default` - оставляет существующие файлы перевода без изменений и записывает только отсутствующие.
- `append` - добавляет новый текст и сохраняет существующие переводы. Файлы, содержимое которых не менялось с прошлого чтения, пропускаются. Переводы для текста, которого больше нет в игре, сохраняются, если не задан `--skip-obsolete`.
- `force` - перезаписывает существующие файлы перевода после подтверждения.
- `force-append` - `append` плюс `force`, который обходит проверку хэшей.

`read` также принимает `-I, --ignore` (применить `.rvpacker-ignore`), `-m, --map-events` (записывать, из каких событий карты пришла каждая строка) и `--skip-obsolete` (алиас `--so`), который отбрасывает устаревшие переводы вместо их сохранения.

`read`, `write` и `purge` используют общие опции:

- `-s, --skip-files <FILES>` (алиас `--skip`) - через запятую перечисленные виды файлов для пропуска. Имена сравниваются точно, без учёта регистра: `map`, `actors`, `armors`, `classes`, `commonevents`, `enemies`, `items`, `skills`, `states`, `system`, `troops`, `weapons`, `scripts`, `database` (только RM2K). `plugins` и `scripts` взаимозаменяемы.
- `--skip-maps <INDICES>` (алиас `--sm`) - ID карт для пропуска, например `0,1,5-8`.
- `--skip-events <SPEC>` (алиас `--se`) - ID событий по файлам для пропуска, в формате `file:0,1,..;file:0,1,..`. Имена файлов должны совпадать точно, например `commonevents`; неизвестное имя приводит к ошибке. На карты не влияет.
- `-d, --duplicate-mode <allow|remove>` (алиас `--dup-mode`).

### `write`

Записывает переведённые `.txt` файлы обратно в исходный формат игры, в `<output-dir>/output`.

```bash
rvpacker-txt-rs write -i "C:/Game"
```

### `purge`

Удаляет записи перевода, которые так и не были заполнены.

```bash
rvpacker-txt-rs purge -i "C:/Game"
```

### `json`

Конвертирует бинарные файлы данных в JSON и обратно, независимо от процесса перевода. Охватывает Marshal-файлы XP/VX/VX Ace и файлы RPG Maker 2000/2003 `RPG_RT.ldb`/`RPG_RT.lmt`/`MapNNNN.lmu`. Не применяется к MV/MZ, которые уже хранят данные в JSON: `json generate` на них завершается с ошибкой.

```bash
rvpacker-txt-rs json generate -i "C:/Game"   # записывает C:/Game/json/*.json (и Scripts.rb)
rvpacker-txt-rs json write -i "C:/Game"      # читает C:/Game/json и записывает в C:/Game/json-output
```

`json generate` пропускает файлы, у которых JSON уже существует. `-r force` (алиас `--mode force`) перезаписывает их.

Для RPG Maker 2000/2003 исходное расширение сохраняется перед `.json` (`RPG_RT.ldb.json`, `RPG_RT.lmt.json`, `MapNNNN.lmu.json`), потому что `RPG_RT.ldb` и `RPG_RT.lmt` иначе совпали бы по имени.

### `serde`

Экспортирует `translation/*.txt` в структурированный формат для редактирования во внешних инструментах и импортирует его обратно. Поддерживает JSON, CSV, XML, XLSX и YAML.

```bash
rvpacker-txt-rs serde export json -i "C:/Game"   # записывает C:/Game/translation/*.txt в C:/Game/serde/*.json
rvpacker-txt-rs serde import json -i "C:/Game"   # читает C:/Game/serde/*.json обратно в C:/Game/translation/*.txt
```

`export` записывает по одному файлу на каждый `.txt` в `<output-dir>/serde`. `import` перезаписывает соответствующий `.txt` для каждого файла нужного формата, найденного там. JSON и YAML хранят каждую строку как запись с меткой `comment` или `translation`. CSV и XLSX используют одну плоскую структуру `type, source, translation_count, translation_1..N`. XML использует элементы `<comment>` и `<entry>`. Точные схемы см. в [модуле `serde`](https://github.com/RPG-Maker-Translation-Tools/rvpacker-txt-rs-lib/blob/main/src/serde.rs) библиотеки.

### `asset`

Расшифровывает и зашифровывает ассеты MV/MZ `.png`, `.ogg` и `.m4a` без разбора игры. То же, что [rpgm-asset-decrypter-rs](https://github.com/rpg-maker-translation-tools/rpgm-asset-decrypter-rs). Использует `-i` и `-o` как директории.

```bash
rvpacker-txt-rs asset decrypt -i "C:/Game/www/img/pictures"   # .rpgmvp/.png_ => .png, рядом со входом или в -o
rvpacker-txt-rs asset encrypt -i "C:/Out" -E mz -e <KEY>      # .png => .rpgmvp (MZ: .png_)
rvpacker-txt-rs asset extract-key -f "C:/Game/www/data/System.json"
```

`decrypt` определяет ключ для каждого файла, если не задан `-e`. `--file` обрабатывает один файл вместо входной директории. `--force` заставляет `decrypt` перезаписывать существующий результат; `encrypt` перезаписывает всегда. Обрабатываются только файлы прямо в `-i`, подпапки не обходятся.

### `archive`

Распаковывает и создаёт архивы `.rgss` RPG Maker XP/VX/VX Ace без разбора игры. То же, что [rpgm-archive-decrypter](https://github.com/RPG-Maker-Translation-Tools/rpgm-archive-decrypter). Использует `-i` как архив или его директорию, а `-o` как выход.

```bash
rvpacker-txt-rs archive decrypt -i "C:/Game"                         # распаковывает .rgss из C:/Game в C:/Game
rvpacker-txt-rs archive encrypt -i "C:/Game" -e vxace                # записывает C:/Game/Game.rgss3a
```

`--encoding` задаёт кодировку имён файлов внутри архива, например `shift_jis` для японских игр XP/VX. `encrypt` по умолчанию упаковывает `data` и `graphics`; используйте `-E` и `--additional-encrypt-dirs`, чтобы это изменить. `encrypt` спрашивает перед перезаписью существующего архива. `decrypt -d` (`--decrypt-files`) извлекает только перечисленные записи, разделённые `:`.

### Глобальные опции

Применяются ко всем командам:

- `-i, --input-dir <PATH>` - по умолчанию `./`.
- `-o, --output-dir <PATH>` - по умолчанию входная директория.
- `--line-separator <SEPARATOR>`, `--line-break <BREAK>`, `--comment-prefix <PREFIX>` - переопределяют значения формата файла перевода (`<#>`, `\#`, `<!>`). См. [`.rvpacker-metadata` и `.rvpacker-ignore`](#rvpacker-metadata-и-rvpacker-ignore).
- `--read-encoding <ENCODING>` и `--write-encoding <ENCODING>` - см. [Кодировка текста](#кодировка-текста).
- `-v`/`-q` - повысить/понизить уровень логирования.

## Сборка

Нужен Rust `1.88`.

```bash
git clone https://github.com/RPG-Maker-Translation-Tools/rvpacker-txt-rs
cd rvpacker-txt-rs
cargo build --release
```

Бинарный файл появится в `target/release/rvpacker-txt-rs` (`rvpacker-txt-rs.exe` на Windows).

## Поддержка

[Я](https://github.com/savannstm), мейнтейнер данного проекта, бедный студентик из Восточной Европы.

Если можете, поддержите нас через:

- [Ko-fi](https://ko-fi.com/savannstm)
- [Patreon](https://www.patreon.com/cw/savannstm)
- [Boosty](https://boosty.to/mcdeimos)

Даже если не поддержите, чёрт с ним. Я буду дальше ерундой своей заниматься.

## Лицензия

Проект лицензирован под [WTFPL](https://www.wtfpl.net/).
