#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use anyhow::{Context, Result, anyhow, bail};
use clap::{
    ArgAction, Args, Parser, Subcommand, ValueEnum,
    builder::{PossibleValuesParser, TypedValueParser},
    crate_version, value_parser,
};
use clap_verbosity_flag::{InfoLevel, Verbosity};
use encoding_rs::{
    BIG5, EUC_KR, Encoding, GB18030, SHIFT_JIS, UTF_8, WINDOWS_1250, WINDOWS_1251, WINDOWS_1252, WINDOWS_1253,
    WINDOWS_1254, WINDOWS_1255, WINDOWS_1256, WINDOWS_1257, WINDOWS_1258,
};
use gxhash::HashMap;
use rpgmad_lib::Decrypter;
use rvpacker_lib::{
    BaseFlags, Mode, Processor, RPGMFileType, RVPACKER_IGNORE_FILE, RVPACKER_METADATA_FILE, get_ini_title,
    get_ini_title_rm2k, json, set_comment_prefix, set_line_break, set_line_separator,
    serde::{
        export_csv, export_json, export_xlsx, export_xml, export_yaml, import_csv, import_json, import_xlsx,
        import_xml, import_yaml,
    },
    types::{DuplicateMode, EngineType, FileFlags},
};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};
use serde_json::{from_str, to_string};
use std::{
    borrow::Cow,
    fs::{create_dir_all, read, read_dir, read_to_string, write},
    io::stdin,
    mem::take,
    path::{Component, Path, PathBuf},
    process::exit,
    str::FromStr,
    time::Instant,
};
use strum::VariantNames;

/// Parses `0,1,5-8` into the indices it spells out. Empty parts are ignored.
fn parse_indices(s: &str) -> Result<Vec<u16>, String> {
    let mut indices = Vec::new();

    for part in s.split(',').map(str::trim).filter(|part| !part.is_empty()) {
        if let Some((a, b)) = part.split_once('-') {
            let (a, b) = (a.trim(), b.trim());
            let start = a
                .parse::<u16>()
                .map_err(|e| format!("Invalid start of range `{a}`: {e}"))?;
            let end = b
                .parse::<u16>()
                .map_err(|e| format!("Invalid end of range `{b}`: {e}"))?;

            if start > end {
                return Err(format!("Range `{part}` is reversed (start > end)"));
            }

            indices.extend(start..=end);
        } else {
            indices.push(
                part.parse::<u16>()
                    .map_err(|e| format!("Invalid integer `{part}`: {e}"))?,
            );
        }
    }

    Ok(indices)
}

#[derive(Clone, Debug)]
pub struct SkipMaps(pub Vec<u16>);

impl FromStr for SkipMaps {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_indices(s).map(SkipMaps)
    }
}

/// Holds the raw `file:0,1,..` file-kind name rather than an already-resolved
/// [`RPGMFileType`]: which variant a name like `"actors"` means depends on the
/// engine (MV/VX's `RPGMFileType::Actors` vs. RM2K's `RPGMFileType::Rm2kActors`),
/// and the engine isn't known yet when arguments are parsed - see
/// [`resolve_file_type`].
#[derive(Clone, Debug)]
pub struct SkipEvents(pub Vec<(String, Vec<u16>)>);

impl FromStr for SkipEvents {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut result = Vec::new();

        for section in s.split(';').map(str::trim).filter(|section| !section.is_empty()) {
            let Some((file, indices)) = section.split_once(':') else {
                return Err(format!("Invalid section `{section}`, expected `file:0,1,..`"));
            };

            result.push((file.trim().to_string(), parse_indices(indices)?));
        }

        Ok(SkipEvents(result))
    }
}

/// Resolves a `--skip-events` file-kind name to the [`RPGMFileType`] variant
/// the given engine actually uses for it.
///
/// Non-RM2K resolve through [`RPGMFileType::from_filename`]'s first-three-bytes
/// matching, same as a real file name would; RM2K has no per-kind file names
/// to match against (`RPG_RT.ldb` bundles every kind into one file), so its
/// names are matched directly against the same names the library's own
/// `.txt` output uses (`actors.txt`, `skills.txt`, ...).
fn resolve_file_type(name: &str, engine_type: EngineType) -> Option<RPGMFileType> {
    if engine_type != EngineType::RM2K {
        let file_type = RPGMFileType::from_filename(name);

        // `from_filename` maps anything it doesn't know to `Invalid`, and maps have no events to skip.
        if matches!(file_type, RPGMFileType::Invalid | RPGMFileType::Map) {
            eprintln!("warning: `{name}` is not a file kind that has events to skip; ignoring it");
            return None;
        }

        return Some(file_type);
    }

    Some(match name.to_ascii_lowercase().as_str() {
        "actors" => RPGMFileType::Rm2kActors,
        "skills" => RPGMFileType::Rm2kSkills,
        "items" => RPGMFileType::Rm2kItems,
        "enemies" => RPGMFileType::Rm2kEnemies,
        "troops" => RPGMFileType::Rm2kTroops,
        "classes" => RPGMFileType::Rm2kClasses,
        "commonevents" => RPGMFileType::Rm2kCommonEvents,
        "states" => RPGMFileType::Rm2kStates,
        "terms" => RPGMFileType::Rm2kTerms,
        _ => {
            eprintln!("warning: `{name}` is not a recognized RPG Maker 2000/2003 file kind; ignoring it");
            return None;
        }
    })
}

#[derive(Clone, Copy, Debug)]
pub struct FFlags(pub FileFlags);

impl FromStr for FFlags {
    type Err = <FileFlags as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut flags = FileFlags::empty();

        for flag_str in s.split(',').filter(|s| !s.is_empty()) {
            let flag = FileFlags::from_str(flag_str)?;
            flags.insert(flag);
        }

        Ok(FFlags(flags))
    }
}

/// Codepages the library can't detect on its own: `Game.ini`'s/`RPG_RT.ini`'s
/// title, and the general text-decoding override ([`Session::encoding`]) that
/// the library needs for the same engines' scripts and for RM2K, none of
/// which record their encoding anywhere in the file.
///
/// There is no way to detect any of these from the bytes alone, so the user
/// has to say which one it is - see `--read-encoding`/`--probe-ini-title`.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum TextEncoding {
    #[value(name = "utf-8")]
    Utf8,
    #[value(name = "shift-jis")]
    ShiftJis,
    #[value(name = "gb18030")]
    Gb18030,
    /// UHC/CP949 - the WHATWG Encoding Standard's "EUC-KR" label is actually
    /// this superset, not the original EUC-KR.
    #[value(name = "euc-kr")]
    EucKr,
    #[value(name = "big5")]
    Big5,
    #[value(name = "windows-1250")]
    Windows1250,
    #[value(name = "windows-1251")]
    Windows1251,
    #[value(name = "windows-1252")]
    Windows1252,
    #[value(name = "windows-1253")]
    Windows1253,
    #[value(name = "windows-1254")]
    Windows1254,
    #[value(name = "windows-1255")]
    Windows1255,
    #[value(name = "windows-1256")]
    Windows1256,
    #[value(name = "windows-1257")]
    Windows1257,
    #[value(name = "windows-1258")]
    Windows1258,
}

impl TextEncoding {
    fn as_encoding(self) -> &'static Encoding {
        match self {
            Self::Utf8 => UTF_8,
            Self::ShiftJis => SHIFT_JIS,
            Self::Gb18030 => GB18030,
            Self::EucKr => EUC_KR,
            Self::Big5 => BIG5,
            Self::Windows1250 => WINDOWS_1250,
            Self::Windows1251 => WINDOWS_1251,
            Self::Windows1252 => WINDOWS_1252,
            Self::Windows1253 => WINDOWS_1253,
            Self::Windows1254 => WINDOWS_1254,
            Self::Windows1255 => WINDOWS_1255,
            Self::Windows1256 => WINDOWS_1256,
            Self::Windows1257 => WINDOWS_1257,
            Self::Windows1258 => WINDOWS_1258,
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    duplicate_mode: DuplicateMode,
    line_separator: Option<String>,
    line_break: Option<String>,
    comment_prefix: Option<String>,
    /// The codepage's canonical WHATWG label (`encoding_rs`'s `Encoding::name()`),
    /// so it round-trips through [`Encoding::for_label`] without a custom
    /// `Serialize`/`Deserialize` impl for [`TextEncoding`].
    ///
    /// Matters on write too, not just read: recovering a source line to look
    /// it up in the translation file requires decoding it the same way it
    /// was decoded when the `.txt` was produced.
    read_encoding: Option<String>,
    /// Whether `read` recorded event headers. `append` and `purge` regenerate the translation files from the game
    /// and drop whatever headers they aren't told to write, so this has to be remembered like the format settings.
    /// Absent in metadata written before it existed, which means off.
    #[serde(default)]
    map_events: bool,
    #[serde(serialize_with = "serialize_sorted_hashes")]
    hashes: Option<HashMap<String, u64>>,
}

fn serialize_sorted_hashes<S>(hashes: &Option<HashMap<String, u64>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match hashes {
        Some(map) => {
            let mut entries: Vec<(&String, &u64)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut m = serializer.serialize_map(Some(entries.len()))?;
            for (k, v) in entries {
                m.serialize_entry(k, v)?;
            }
            m.end()
        }
        None => serializer.serialize_none(),
    }
}

/// Resolves a codepage's canonical WHATWG label (as stored in
/// `.rvpacker-metadata`/produced by [`TextEncoding::as_encoding`]) back to an
/// [`Encoding`], for [`Processor::read_encoding`]/[`Processor::write_encoding`].
fn resolve_encoding(label: Option<&str>) -> Option<&'static Encoding> {
    let label = label?;
    let encoding = Encoding::for_label(label.as_bytes());

    if encoding.is_none() {
        eprintln!("warning: `{label}` is not a recognized encoding; ignoring it");
    }

    encoding
}

fn apply_format_overrides(
    line_separator: Option<&String>,
    line_break: Option<&String>,
    comment_prefix: Option<&String>,
) {
    if let Some(separator) = line_separator {
        set_line_separator(Box::leak(separator.clone().into_boxed_str()));
    }

    if let Some(line_break) = line_break {
        set_line_break(Box::leak(line_break.clone().into_boxed_str()));
    }

    if let Some(prefix) = comment_prefix {
        set_comment_prefix(Box::leak(prefix.clone().into_boxed_str()));
    }
}

#[derive(Args, Debug)]
struct SharedArgs {
    /// Skips processing specified files, separated by comma. `plugins` can be used interchangeably with `scripts`
    #[arg(
        short,
        long,
        alias = "skip",
        value_name = "FILES",
        display_order = 94,
        default_value = "",
        value_parser = value_parser!(FFlags)
    )]
    skip_files: FFlags,

    /// Skips processing specified maps, separated by comma.
    #[arg(
        long,
        alias = "sm",
        value_name = "MAP_INDICES",
        value_parser = value_parser!(SkipMaps),
        default_value = ""
    )]
    skip_maps: SkipMaps,

    /// Skips processing specified events. Has no effect on maps.
    /// Follows the following syntax: `file:0,1,..;file:0,1,..`
    #[arg(
        long,
        alias = "se",
        value_name = "EVENT_INDICES",
        value_parser = value_parser!(SkipEvents),
        default_value = ""
    )]
    skip_events: SkipEvents,

    /// Controls how to handle duplicates in text. Defaults to `allow`.
    /// Left unset, the value recorded in `.rvpacker-metadata` is used.
    #[arg(
        short,
        long,
        alias = "dup-mode",
        display_order = 93,
        value_parser = PossibleValuesParser::new(DuplicateMode::VARIANTS).map(|s| DuplicateMode::from_str(&s).unwrap())
    )]
    duplicate_mode: Option<DuplicateMode>,
}

#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)]
struct ReadArgs {
    /// Defines how to read files.
    /// `default` - If encounters existing translation files, aborts read.
    /// `append` - Appends any new text from the game to the translation files, if the text is not already present. Unused lines are removed from translation files, and the lines order is sorted.
    /// `force` - Force rewrites existing translation files
    #[arg(
        short,
        long,
        alias = "mode",
        default_value = "default",
        value_name = "MODE",
        display_order = 3,
        value_parser = PossibleValuesParser::new(["default", "append", "force", "force-append"]).map(|s| Mode::from_str(&s).unwrap())
    )]
    read_mode: Mode,

    #[arg(short = 'S', long, hide = true, action = ArgAction::SetTrue)]
    silent: bool,

    /// Ignore entries from `.rvpacker-ignore` file.
    #[arg(short = 'I', long, action = ArgAction::SetTrue)]
    ignore: bool,

    #[arg(long, alias = "so", action = ArgAction::SetTrue)]
    skip_obsolete: bool,

    /// Parse information about map events for the map files.
    /// This will give you more info about where the text of the event happens.
    /// Recorded in `.rvpacker-metadata`, so `append` and `purge` keep the event headers without it.
    #[arg(short, long, alias = "me", action = ArgAction::SetTrue)]
    map_events: bool,

    /// Decodes `Game.ini`'s (or, if absent, `RPG_RT.ini`'s) title with the
    /// given encoding and prints it, without reading anything else. Use this
    /// to find the right encoding before passing it to `--read-encoding`.
    #[arg(long, value_name = "ENCODING")]
    probe_ini_title: Option<TextEncoding>,

    /// Whether to parse the game's title out of `Game.ini`/`RPG_RT.ini` at
    /// all. Left unset, neither file is touched. When set, the title is
    /// decoded with `--read-encoding` - overriding whatever the system file's
    /// own title field carries. Guessing the wrong encoding would silently
    /// corrupt it, so this stays opt-in.
    #[arg(long, action = ArgAction::SetTrue)]
    use_ini_title: bool,

    #[command(flatten)]
    shared: SharedArgs,
}

#[derive(Args, Debug)]
struct PurgeArgs {
    /// Creates an ignore file from purged lines, to prevent their further appearance when reading with `--mode append`
    #[arg(short, long, action = ArgAction::SetTrue, display_order = 23)]
    create_ignore: bool,

    /// Keeps the event headers of the map files. Not needed if `read` was run with `--map-events`: it's recorded in
    /// `.rvpacker-metadata`.
    #[arg(short, long, alias = "me", action = ArgAction::SetTrue)]
    map_events: bool,

    #[command(flatten)]
    shared: SharedArgs,
}

#[derive(Debug, Subcommand)]
enum JsonSubcommand {
    /// Generates JSON representations of older engines' files in `json` directory
    Generate {
        /// `default` - Skips files whose JSON already exists. `force` - Overwrites them
        #[arg(
            short,
            long,
            alias = "mode",
            default_value = "default",
            value_name = "MODE",
            value_parser = PossibleValuesParser::new(["default", "force"]).map(|s| Mode::from_str(&s).unwrap())
        )]
        read_mode: Mode,
    },

    /// Writes JSON representations of older engines' files from `json` directory back to original files
    Write,
}

/// A structured format translation `.txt` files can be exported to/imported from - see [`SerdeSubcommand`].
#[derive(Clone, Copy, Debug, ValueEnum)]
enum SerdeFormat {
    Json,
    Csv,
    Xml,
    Xlsx,
    Yaml,
}

impl SerdeFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Csv => "csv",
            Self::Xml => "xml",
            Self::Xlsx => "xlsx",
            Self::Yaml => "yaml",
        }
    }
}

#[derive(Debug, Subcommand)]
enum SerdeSubcommand {
    /// Exports translation `.txt` files to a structured format, for editing in external tools (spreadsheet
    /// editors, XML/YAML-aware editors, ...)
    Export {
        /// Format to export the translation files to
        format: SerdeFormat,
    },

    /// Imports a structured format created with `serde export` back into translation `.txt` files
    Import {
        /// Format to import the translation files from
        format: SerdeFormat,
    },
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Parses game files to `.txt` format, and decrypts any `.rgss` archive if it's present
    Read(ReadArgs),

    /// Writes translated game files to the output directory
    Write(SharedArgs),

    /// Purges lines without translation from translation files
    Purge(PurgeArgs),

    /// Provides the commands for JSON generation and writing
    Json {
        #[command(subcommand)]
        subcommand: JsonSubcommand,
    },

    /// Provides the commands for exporting/importing translation files to/from structured formats
    /// (JSON, CSV, XML, XLSX, YAML)
    Serde {
        #[command(subcommand)]
        subcommand: SerdeSubcommand,
    },
}

/// This tool allows to parse RPG Maker 2000/2003/XP/VX/VXAce/MV/MZ games text to `.txt` files and write them back to their initial form. For MV/MZ/XP/VX/VXAce, the program uses `data` or `Data` directories for source files and decrypts any `.rgss` archive if it's present; for RPG Maker 2000/2003, it looks for `RPG_RT.ldb` directly in the input directory. `translation` directory holds the translation files either way.
#[derive(Debug, Parser)]
#[command(version = crate_version!(), next_line_help = true, term_width = 120)]
struct Cli {
    /// Input directory, containing game files
    #[arg(short, long, global = true, default_value = "./", value_name = "INPUT_PATH", value_parser = value_parser!(PathBuf), display_order = 1)]
    input_dir: PathBuf,

    /// Output directory to output files to
    #[arg(short, long, global = true, value_name = "OUTPUT_PATH", value_parser = value_parser!(PathBuf), display_order = 2)]
    output_dir: Option<PathBuf>,

    /// Separator between the source and translation columns of a translation line.
    /// Defaults to the library's own default (`<#>`). Left unset, the value recorded
    /// in `.rvpacker-metadata` is used.
    #[arg(long, global = true, value_name = "SEPARATOR", display_order = 4)]
    line_separator: Option<String>,

    /// Marker that source/translation line breaks are normalized to in translation files.
    /// Defaults to the library's own default (`\#`). Left unset, the value recorded
    /// in `.rvpacker-metadata` is used.
    #[arg(long, global = true, value_name = "BREAK", display_order = 5)]
    line_break: Option<String>,

    /// Prefix marking a line in translation files as a comment (`ID`, `NAME`, ignore
    /// entries, etc). Defaults to the library's own default (`<!>`). Left unset, the
    /// value recorded in `.rvpacker-metadata` is used.
    #[arg(long, global = true, value_name = "PREFIX", display_order = 6)]
    comment_prefix: Option<String>,

    /// Forces decoding of the game's own source text to the given codepage,
    /// instead of guessing it. Needed for RPG Maker 2000/2003 (whose files
    /// carry no encoding at all) and XP/VX (same, plus `Scripts.*`) - without
    /// it, the library just tries a fixed list of common codepages and keeps
    /// the first one that decodes without errors. VX Ace tags most strings
    /// with their real encoding already and only falls back to this for the
    /// rare one that doesn't. Matters on write as well as read - recovering a
    /// source line to look it up in the translation file needs the same
    /// decoding that produced that `.txt` in the first place.
    ///
    /// Tip: `--probe-ini-title`/`--use-ini-title` can help find the right value
    /// - `Game.ini`'s/`RPG_RT.ini`'s title is the one field in that file that
    /// can carry non-ASCII bytes, and when it does, it was written by RPG
    /// Maker's editor in the original developer's own codepage - the same
    /// one the rest of the game's text uses.
    ///
    /// Independent of `--write-encoding` - see its help text for why. Left unset,
    /// the value recorded in `.rvpacker-metadata` is used.
    #[arg(long, global = true, value_name = "ENCODING", display_order = 7)]
    read_encoding: Option<TextEncoding>,

    /// Forces encoding of translated text to the given codepage when writing
    /// it back, instead of always writing plain UTF-8.
    ///
    /// Leave this unset. UTF-8 is the only choice that can't silently corrupt
    /// a translation written in a different script than the source game's -
    /// a Japanese (`Shift_JIS`) game translated into Russian cannot be
    /// re-encoded as `Shift_JIS` at all, and attempting it doesn't fail loudly:
    /// per the WHATWG encoding standard, an unmappable character is replaced
    /// with a literal decimal numeric-character-reference (`П` becomes the
    /// eight bytes `&#1055;`) spliced into the output.
    ///
    /// Only override this if the target engine build has no Unicode-aware
    /// text renderer - true of RPG Maker 2000/2003, XP and VX, which render
    /// through the OS's legacy ANSI codepage rather than decoding UTF-8 - AND
    /// the translation's own script fits inside the codepage you pass here.
    /// Whoever runs the translated game then also needs their system (or a
    /// locale emulator) set to that same codepage. VX Ace is Unicode-native,
    /// so its UTF-8 default already works correctly without this.
    ///
    /// Independent of `--read-encoding`. Never recorded in `.rvpacker-metadata`,
    /// so pass it on every `write`.
    #[arg(long, global = true, value_name = "ENCODING", display_order = 8)]
    write_encoding: Option<TextEncoding>,

    #[command(subcommand)]
    command: Command,

    #[command(flatten)]
    verbosity: Verbosity<InfoLevel>,
}

fn parse_metadata(metadata_file_path: &Path) -> Result<Option<Metadata>> {
    if !metadata_file_path.exists() {
        return Ok(None);
    }

    let metadata_file_content =
        read_to_string(metadata_file_path).with_context(|| metadata_file_path.display().to_string())?;
    let metadata = from_str(&metadata_file_content).with_context(|| metadata_file_path.display().to_string())?;
    Ok(Some(metadata))
}

struct Session<'a> {
    engine_type: EngineType,

    input_dir: PathBuf,
    system_file_path: PathBuf,
    game_ini_path: PathBuf,
    rpg_rt_ini_path: PathBuf,
    metadata_file_path: PathBuf,

    source_path: PathBuf,
    translation_path: PathBuf,
    ignore_file_path: PathBuf,

    archive_path: Option<PathBuf>,
    output_dir: PathBuf,

    line_separator: Option<String>,
    line_break: Option<String>,
    comment_prefix: Option<String>,
    /// The codepage's canonical WHATWG label - see [`Metadata::read_encoding`].
    read_encoding: Option<String>,
    write_encoding: Option<String>,

    start_time: &'a mut Instant,
}

impl<'a> Session<'a> {
    pub fn new(cli: &mut Cli, start_time: &'a mut Instant) -> Result<Self, anyhow::Error> {
        let input_dir = take(&mut cli.input_dir);

        if !input_dir.exists() {
            bail!("Input directory does not exist.");
        }

        let output_dir = take(&mut cli.output_dir).unwrap_or_else(|| input_dir.clone());

        if !output_dir.exists() {
            bail!("Output directory does not exist.");
        }

        let translation_path = output_dir.join("translation");
        let metadata_file_path = translation_path.join(RVPACKER_METADATA_FILE);
        let ignore_file_path = translation_path.join(RVPACKER_IGNORE_FILE);

        // RPG Maker 2000/2003 keeps every file at the project root - `RPG_RT.ldb`,
        // rather than a `System.*` file in a `data`/`Data` directory, is what
        // marks a project as one. Checked first because there's no `data`/`Data`
        // directory to require in that case.
        let rm2k_database_path = input_dir.join("RPG_RT.ldb");

        let (engine_type, source_path, system_file_path, archive_path) = if rm2k_database_path.exists() {
            (EngineType::RM2K, input_dir.clone(), rm2k_database_path, None)
        } else {
            // Games that ship only an `.rgss` archive have no data directory until `read` extracts it, and
            // that's where the archive puts it.
            let source_path = ["data", "Data"]
                .into_iter()
                .map(|dir| input_dir.join(dir))
                .find(|path| path.exists())
                .unwrap_or_else(|| input_dir.join("Data"));

            let type_paths = [
                (EngineType::MVMZ, source_path.join("System.json"), None),
                (
                    EngineType::VXAce,
                    source_path.join("System.rvdata2"),
                    Some(input_dir.join("Game.rgss3a")),
                ),
                (
                    EngineType::VX,
                    source_path.join("System.rvdata"),
                    Some(input_dir.join("Game.rgss2a")),
                ),
                (
                    EngineType::XP,
                    source_path.join("System.rxdata"),
                    Some(input_dir.join("Game.rgssad")),
                ),
            ];

            let Some((engine_type, system_file_path, archive_path)) =
                type_paths
                    .into_iter()
                    .find_map(|(engine_type, system_file_path, archive_path)| {
                        if !system_file_path.exists() && archive_path.as_ref().is_none_or(|path| !path.exists()) {
                            return None;
                        }

                        Some((engine_type, system_file_path, archive_path))
                    })
            else {
                bail!(
                    "Couldn't determine game engine. Check the existence of `System` file inside `data`/`Data` \
                     directory, `.rgss` archive, or `RPG_RT.ldb` in the input directory ({}).",
                    input_dir.display()
                );
            };

            (engine_type, source_path, system_file_path, archive_path)
        };

        let game_ini_path = input_dir.join("Game.ini");
        let rpg_rt_ini_path = input_dir.join("RPG_RT.ini");

        let line_separator = take(&mut cli.line_separator);
        let line_break = take(&mut cli.line_break);
        let comment_prefix = take(&mut cli.comment_prefix);
        let read_encoding = take(&mut cli.read_encoding).map(|e| e.as_encoding().name().to_string());
        let write_encoding = take(&mut cli.write_encoding).map(|e| e.as_encoding().name().to_string());

        Ok(Self {
            engine_type,
            input_dir,
            system_file_path,
            game_ini_path,
            rpg_rt_ini_path,
            metadata_file_path,
            source_path,
            translation_path,
            ignore_file_path,
            archive_path,
            output_dir,
            line_separator,
            line_break,
            comment_prefix,
            read_encoding,
            write_encoding,
            start_time,
        })
    }

    /// Reads whichever of `Game.ini`/`RPG_RT.ini` is present - probing
    /// `Game.ini` first - and extracts the raw title bytes from it.
    fn read_ini_title(&self) -> Result<Vec<u8>, anyhow::Error> {
        if self.engine_type.is_mvmz() {
            bail!(
                "`Game.ini`/`RPG_RT.ini` title parsing has no effect on MV/MZ, which carries its title in \
                 `System.json`"
            );
        }

        if self.game_ini_path.exists() {
            let content = read(&self.game_ini_path).with_context(|| self.game_ini_path.display().to_string())?;
            Ok(get_ini_title(&content)?)
        } else if self.rpg_rt_ini_path.exists() {
            let content = read(&self.rpg_rt_ini_path).with_context(|| self.rpg_rt_ini_path.display().to_string())?;
            Ok(get_ini_title_rm2k(&content)?)
        } else {
            bail!(
                "Neither {} nor {} exists.",
                self.game_ini_path.display(),
                self.rpg_rt_ini_path.display()
            );
        }
    }

    fn ini_game_title(&self, use_ini_title: bool, encoding: Option<&'static Encoding>) -> String {
        if !use_ini_title {
            return String::new();
        }

        let Some(encoding) = encoding else {
            eprintln!("warning: --use-ini-title has no effect without --read-encoding, which decodes the title");
            return String::new();
        };

        match self.read_ini_title() {
            Ok(title) => {
                let (title, _, had_errors) = encoding.decode(&title);

                if had_errors {
                    eprintln!(
                        "warning: decoding the title as {} produced replacement characters - this is probably not the \
                         right encoding",
                        encoding.name()
                    );
                }

                title.into_owned()
            }
            Err(err) => {
                eprintln!("warning: {err}");
                String::new()
            }
        }
    }

    /// Decodes `Game.ini`'s (or, if absent, `RPG_RT.ini`'s) title with
    /// `encoding` and prints it, for `--probe-ini-title`. Reads nothing else.
    fn print_ini_title(&self, encoding: TextEncoding) -> Result<(), anyhow::Error> {
        let title_bytes = self.read_ini_title()?;
        let (title, _, had_errors) = encoding.as_encoding().decode(&title_bytes);

        println!("{title}");

        if had_errors {
            eprintln!("warning: decoding produced replacement characters - this is probably not the right encoding");
        }

        Ok(())
    }

    /// The settings to run with: what was passed explicitly on the command line, else what `.rvpacker-metadata`
    /// recorded, else the defaults. `recorded` is [`None`] when there's no metadata to consult.
    ///
    /// The settings change the translation file format, so the recorded ones have to win over anything the user
    /// didn't ask for - but a flag that was passed always wins, and a recorded `hashes` is passed through untouched.
    fn merge_metadata(
        &self,
        duplicate_mode: Option<DuplicateMode>,
        map_events: bool,
        recorded: Option<Metadata>,
    ) -> Metadata {
        let recorded = recorded.unwrap_or_default();

        Metadata {
            duplicate_mode: duplicate_mode.unwrap_or(recorded.duplicate_mode),
            line_separator: self.line_separator.clone().or(recorded.line_separator),
            line_break: self.line_break.clone().or(recorded.line_break),
            comment_prefix: self.comment_prefix.clone().or(recorded.comment_prefix),
            read_encoding: self.read_encoding.clone().or(recorded.read_encoding),
            // A flag can only turn the headers on. To turn them off, delete `.rvpacker-metadata`.
            map_events: map_events || recorded.map_events,
            hashes: recorded.hashes,
        }
    }

    /// Extracts the game's `.rgss` archive into the input directory, unless the system file is already there.
    ///
    /// Entry names are decoded with `encoding` (UTF-8 when unset) - the same codepage the game's text is in, which
    /// is also what older archives use for their names. Entries that don't decode are skipped with a warning
    /// rather than written under a mangled name that could collide with another entry.
    fn extract_archive(&self, encoding: Option<&'static Encoding>) -> Result<(), anyhow::Error> {
        let Some(archive_path) = &self.archive_path else {
            return Ok(());
        };

        if self.system_file_path.exists() {
            return Ok(());
        }

        let encoding = encoding.unwrap_or(UTF_8);
        let mut archive_data = read(archive_path).with_context(|| archive_path.display().to_string())?;
        let mut decrypter = Decrypter::new();
        let mut skipped: usize = 0;

        for file in decrypter.decrypt(&mut archive_data)? {
            let Some(decoded) = encoding.decode_without_bom_handling_and_without_replacement(file.path) else {
                eprintln!(
                    "warning: skipped an archive entry, its name isn't valid {}: {}",
                    encoding.name(),
                    String::from_utf8_lossy(file.path)
                );
                skipped += 1;
                continue;
            };

            // Archives store paths with backslashes. Forward slashes work on every platform, including Windows.
            let path = if decoded.contains('\\') {
                Cow::Owned(decoded.replace('\\', "/"))
            } else {
                decoded
            };

            // The archive is untrusted: an absolute or `..` path would otherwise write outside the input directory.
            if path.is_empty()
                || !Path::new(&*path)
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
            {
                bail!("Archive entry {path:?} points outside the input directory.");
            }

            let output_file_path = self.input_dir.join(&*path);

            if let Some(parent) = output_file_path.parent() {
                create_dir_all(parent)?;
            }

            write(&output_file_path, file.data).with_context(|| output_file_path.display().to_string())?;
        }

        if skipped > 0 {
            eprintln!(
                "warning: {skipped} archive entries were skipped. If they matter, pass the right codepage to \
                 `--read-encoding` and run again."
            );
        }

        Ok(())
    }

    pub fn execute_read(&mut self, args: ReadArgs) -> Result<(), anyhow::Error> {
        if let Some(encoding) = args.probe_ini_title {
            return self.print_ini_title(encoding);
        }

        let silent = args.silent;
        let ignore = args.ignore;
        let skip_obsolete = args.skip_obsolete;
        let use_ini_title = args.use_ini_title;
        let read_mode = args.read_mode;
        let cli_map_events = args.map_events;

        let SharedArgs {
            skip_files,
            duplicate_mode,
            skip_maps,
            skip_events,
        } = args.shared;

        let file_flags = FileFlags::all() & !skip_files.0;

        let Mode::Read { append, force } = read_mode else {
            unreachable!("the read-mode parser only accepts `Read` variants")
        };

        // Without `append` the metadata is only a source of settings, so a broken file must not stop a re-read that
        // is about to replace it.
        let recorded = match parse_metadata(&self.metadata_file_path) {
            Ok(recorded) => recorded,
            Err(error) if !append => {
                eprintln!("warning: ignoring the unreadable `.rvpacker-metadata`: {error:#}");
                None
            }
            Err(error) => return Err(error),
        };

        let Metadata {
            duplicate_mode,
            line_separator,
            line_break,
            comment_prefix,
            read_encoding,
            map_events,
            hashes,
        } = self.merge_metadata(duplicate_mode, cli_map_events, recorded);

        apply_format_overrides(line_separator.as_ref(), line_break.as_ref(), comment_prefix.as_ref());

        // The hashes decide which files are skipped as unchanged, so only an append may build on the recorded ones.
        let hashes = if append { hashes.unwrap_or_default() } else { HashMap::default() };

        if force && !silent {
            let start = Instant::now();
            println!("WARNING! Force mode will forcefully rewrite all your translation files. Input 'Y' to continue.");

            let mut buf = String::with_capacity(4);
            stdin().read_line(&mut buf)?;

            if buf.trim_end() != "Y" {
                println!("Aborted.");
                exit(0);
            }

            // The time spent waiting for the answer isn't the tool's.
            *self.start_time += start.elapsed();
        }

        if append && ignore && !self.ignore_file_path.exists() {
            bail!("`.rvpacker-ignore` file does not exist. Aborting execution.");
        }

        let resolved_read_encoding = resolve_encoding(read_encoding.as_deref());

        self.extract_archive(resolved_read_encoding)?;

        let mut flags = BaseFlags::empty();
        flags.set(BaseFlags::Ignore, ignore);
        flags.set(BaseFlags::SkipObsolete, skip_obsolete);

        let mut processor = Processor {
            mode: read_mode,
            file_flags,
            flags,
            duplicate_mode,
            game_title: self.ini_game_title(use_ini_title, resolved_read_encoding),
            hashes,
            skip_maps: skip_maps.0,
            skip_events: skip_events
                .0
                .into_iter()
                .filter_map(|(name, ids)| resolve_file_type(&name, self.engine_type).map(|ft| (ft, ids)))
                .collect(),
            map_events,
            read_encoding: resolved_read_encoding,
            write_encoding: None,
        };

        processor.process(self.engine_type, &self.source_path, &self.translation_path, None)?;

        let metadata = Metadata {
            duplicate_mode,
            line_separator,
            line_break,
            comment_prefix,
            read_encoding,
            map_events,
            hashes: Some(processor.hashes),
        };

        create_dir_all(&self.translation_path)?;
        write(&self.metadata_file_path, to_string(&metadata)?)?;

        Ok(())
    }

    pub fn execute_write(&self, args: SharedArgs) -> Result<(), anyhow::Error> {
        if !self.translation_path.exists() {
            bail!("`{}` directory does not exist.", self.translation_path.display());
        }

        let SharedArgs {
            skip_files,
            duplicate_mode,
            skip_maps,
            skip_events,
        } = args;

        let file_flags = FileFlags::all() & !skip_files.0;
        let write_encoding = self.write_encoding.clone();

        // Event headers are comments in the translation files, `write` has nothing to do with them.
        let Metadata {
            duplicate_mode,
            line_separator,
            line_break,
            comment_prefix,
            read_encoding,
            ..
        } = self.merge_metadata(duplicate_mode, false, parse_metadata(&self.metadata_file_path)?);

        apply_format_overrides(line_separator.as_ref(), line_break.as_ref(), comment_prefix.as_ref());

        let mut processor = Processor {
            mode: Mode::Write,
            file_flags,
            flags: BaseFlags::empty(),
            duplicate_mode,
            skip_maps: skip_maps.0,
            skip_events: skip_events
                .0
                .into_iter()
                .filter_map(|(name, ids)| resolve_file_type(&name, self.engine_type).map(|ft| (ft, ids)))
                .collect(),
            read_encoding: resolve_encoding(read_encoding.as_deref()),
            write_encoding: resolve_encoding(write_encoding.as_deref()),
            ..Default::default()
        };

        processor.process(
            self.engine_type,
            &self.source_path,
            &self.translation_path,
            Some(&self.output_dir.join("output")),
        )?;

        Ok(())
    }

    pub fn execute_purge(&self, args: PurgeArgs) -> Result<(), anyhow::Error> {
        let SharedArgs {
            skip_files,
            duplicate_mode,
            skip_maps,
            skip_events,
        } = args.shared;

        let file_flags = FileFlags::all() & !skip_files.0;
        let create_ignore = args.create_ignore;

        // Without the headers on, purge rewrites the map files and drops the ones `read --map-events` wrote.
        let Metadata {
            duplicate_mode,
            line_separator,
            line_break,
            comment_prefix,
            read_encoding,
            map_events,
            ..
        } = self.merge_metadata(duplicate_mode, args.map_events, parse_metadata(&self.metadata_file_path)?);

        apply_format_overrides(line_separator.as_ref(), line_break.as_ref(), comment_prefix.as_ref());

        let mut flags = BaseFlags::empty();
        flags.set(BaseFlags::CreateIgnore, create_ignore);

        let mut processor = Processor {
            mode: Mode::Purge,
            file_flags,
            flags,
            duplicate_mode,
            skip_maps: skip_maps.0,
            skip_events: skip_events
                .0
                .into_iter()
                .filter_map(|(name, ids)| resolve_file_type(&name, self.engine_type).map(|ft| (ft, ids)))
                .collect(),
            map_events,
            read_encoding: resolve_encoding(read_encoding.as_deref()),
            write_encoding: None,
            ..Default::default()
        };

        processor.process(self.engine_type, &self.source_path, &self.translation_path, None)?;

        Ok(())
    }

    pub fn execute_json(&self, subcommand: &JsonSubcommand) -> Result<(), anyhow::Error> {
        use json::{generate, generate_rm2k, write, write_rm2k};

        apply_format_overrides(
            self.line_separator.as_ref(),
            self.line_break.as_ref(),
            self.comment_prefix.as_ref(),
        );

        let json_path = self.input_dir.join("json");
        let json_output_path = self.input_dir.join("json-output");

        if self.engine_type == EngineType::RM2K {
            match subcommand {
                JsonSubcommand::Generate { read_mode } => {
                    let force = matches!(read_mode, Mode::Read { force: true, .. });
                    generate_rm2k(&self.source_path, &json_path, force)?;
                }
                JsonSubcommand::Write => {
                    write_rm2k(json_path, json_output_path)?;
                }
            }

            return Ok(());
        }

        match subcommand {
            JsonSubcommand::Generate { read_mode } => {
                let force = matches!(read_mode, Mode::Read { force: true, .. });
                self.extract_archive(resolve_encoding(self.read_encoding.as_deref()))?;
                generate(&self.source_path, &json_path, force)?;
            }
            JsonSubcommand::Write => {
                write(json_path, json_output_path, self.engine_type)?;
            }
        }

        Ok(())
    }

    /// Directory that holds files exported by `serde export`/read back by `serde import` -
    /// mirrors the `json`/`json-output` convention [`Session::execute_json`] uses.
    fn serde_dir(&self) -> PathBuf {
        self.output_dir.join("serde")
    }

    pub fn execute_serde(&self, subcommand: &SerdeSubcommand) -> Result<(), anyhow::Error> {
        if !self.translation_path.exists() {
            bail!("`{}` directory does not exist.", self.translation_path.display());
        }

        let serde_path = self.serde_dir();

        match subcommand {
            SerdeSubcommand::Export { format } => {
                create_dir_all(&serde_path)?;

                for entry in read_dir(&self.translation_path)?.flatten() {
                    let path = entry.path();

                    if path.extension().and_then(|ext| ext.to_str()) != Some("txt") {
                        continue;
                    }

                    let stem = path.file_stem().unwrap().to_string_lossy();
                    let output_path = serde_path.join(format!("{stem}.{}", format.extension()));
                    let content = read_to_string(&path).with_context(|| path.display().to_string())?;

                    let exported = match format {
                        SerdeFormat::Json => export_json(&content).map(String::into_bytes),
                        SerdeFormat::Csv => export_csv(&content).map(String::into_bytes),
                        SerdeFormat::Xml => export_xml(&content).map(String::into_bytes),
                        SerdeFormat::Xlsx => export_xlsx(&content),
                        SerdeFormat::Yaml => export_yaml(&content).map(String::into_bytes),
                    }
                    .map_err(|e| anyhow!("{}: {e}", path.display()))?;

                    write(&output_path, exported).with_context(|| output_path.display().to_string())?;
                    println!("{}: Successfully exported.", output_path.display());
                }
            }
            SerdeSubcommand::Import { format } => {
                if !serde_path.exists() {
                    bail!(
                        "`{}` directory does not exist. Run `serde export` first.",
                        serde_path.display()
                    );
                }

                // Convert everything before writing anything, so that a broken file doesn't leave the translation
                // directory half-imported.
                let mut imported = Vec::new();

                for entry in read_dir(&serde_path)?.flatten() {
                    let path = entry.path();

                    if path.extension().and_then(|ext| ext.to_str()) != Some(format.extension()) {
                        continue;
                    }

                    let stem = path.file_stem().unwrap().to_string_lossy();
                    let output_path = self.translation_path.join(format!("{stem}.txt"));

                    let text = || read_to_string(&path).with_context(|| path.display().to_string());

                    let content = match format {
                        SerdeFormat::Json => import_json(&text()?),
                        SerdeFormat::Csv => import_csv(&text()?),
                        SerdeFormat::Xml => import_xml(&text()?),
                        SerdeFormat::Xlsx => import_xlsx(&read(&path).with_context(|| path.display().to_string())?),
                        SerdeFormat::Yaml => import_yaml(&text()?),
                    }
                    .map_err(|e| anyhow!("{}: {e}", path.display()))?;

                    imported.push((output_path, content));
                }

                for (output_path, content) in imported {
                    write(&output_path, content).with_context(|| output_path.display().to_string())?;
                    println!("{}: Successfully imported.", output_path.display());
                }
            }
        }

        Ok(())
    }
}

fn main() -> Result<()> {
    let mut start_time = Instant::now();
    let mut cli = Cli::parse();

    tracing_subscriber::fmt()
        .without_time()
        .with_target(false)
        .with_level(true)
        .with_thread_names(false)
        .with_thread_ids(false)
        .with_ansi(true)
        .with_max_level(cli.verbosity)
        .init();

    let mut session = Session::new(&mut cli, &mut start_time)?;

    match cli.command {
        Command::Read(args) => session.execute_read(args)?,
        Command::Write(args) => session.execute_write(args)?,
        Command::Purge(args) => session.execute_purge(args)?,
        Command::Json { subcommand } => session.execute_json(&subcommand)?,
        Command::Serde { subcommand } => session.execute_serde(&subcommand)?,
    }

    println!("Elapsed: {:.2}s", start_time.elapsed().as_secs_f32());
    Ok(())
}
