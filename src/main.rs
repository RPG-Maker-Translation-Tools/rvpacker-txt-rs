#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use anyhow::{Context, Result, anyhow, bail};
use clap::{
    ArgAction, Args, Parser, Subcommand, ValueEnum,
    builder::{PossibleValuesParser, TypedValueParser},
    crate_version, value_parser,
};
use clap_verbosity_flag::{InfoLevel, Verbosity};
use encoding_rs::{Encoding, UTF_8};
use rpgmad_lib::decrypt_from_reader;
use rvpacker_lib::{
    BaseFlags, HashMap, Mode, Processor, RPGMFileType, RVPACKER_IGNORE_FILE, RVPACKER_METADATA_FILE, get_ini_title,
    get_ini_title_rm2k, json,
    serde::{
        export_csv, export_json, export_xlsx, export_xml, export_yaml, import_csv, import_json, import_xlsx,
        import_xml, import_yaml,
    },
    set_comment_prefix, set_line_break, set_line_separator,
    types::{DuplicateMode, EngineType, FileFlags},
};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};
use serde_json::{from_str, to_string};
use std::{
    borrow::Cow,
    fs::{File, create_dir_all, read, read_dir, read_to_string, write},
    io::{BufReader, copy, stdin},
    mem::take,
    path::{Component, Path, PathBuf},
    str::FromStr,
    time::Instant,
};

mod archive;
mod asset;

use archive::{ArchiveCommand, execute as execute_archive};
use asset::{AssetCommand, AssetOptions, execute as execute_asset};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Parses `0,1,5-8` into the indices it spells out. Empty parts are ignored.
fn parse_indices(s: &str) -> Result<Vec<u32>, String> {
    let mut indices = Vec::new();

    for part in s.split(',').map(str::trim).filter(|part| !part.is_empty()) {
        if let Some((a, b)) = part.split_once('-') {
            let (a, b) = (a.trim(), b.trim());
            let start = a
                .parse::<u32>()
                .map_err(|e| format!("Invalid start of range `{a}`: {e}"))?;
            let end = b
                .parse::<u32>()
                .map_err(|e| format!("Invalid end of range `{b}`: {e}"))?;

            if start > end {
                return Err(format!("Range `{part}` is reversed (start > end)"));
            }

            // deal with allocating billions of entries

            indices.extend(start..=end);
        } else {
            indices.push(
                part.parse::<u32>()
                    .map_err(|e| format!("Invalid integer `{part}`: {e}"))?,
            );
        }
    }

    Ok(indices)
}

#[derive(Clone, Debug)]
pub struct SkipMaps(pub Vec<u32>);

impl FromStr for SkipMaps {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_indices(s).map(SkipMaps)
    }
}

/// Raw file-kind name, not yet an [`RPGMFileType`]: the engine that gives it meaning isn't known at parse time.
/// See [`resolve_file_type`].
#[derive(Clone, Debug)]
pub struct SkipEvents(pub Vec<(String, Vec<u32>)>);

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

        Ok(Self(result))
    }
}

const SKIP_EVENT_NAMES: &str = "actors, armors, classes, commonevents, enemies, items, skills, states, troops, weapons";
const RM2K_SKIP_EVENT_NAMES: &str = "actors, classes, commonevents, enemies, items, skills, states, terms, troops";

/// Resolves a `--skip-events` file-kind name to the engine's [`RPGMFileType`] variant.
///
/// Names must match exactly: prefix matching would let a typo like `acts` mean `actors`. An unknown name is an error,
/// since skipping nothing silently would leave events in the output that the user meant to skip.
fn resolve_file_type(name: &str, engine_type: EngineType) -> Result<RPGMFileType, String> {
    let file_type = match (engine_type == EngineType::RM2K, name.to_ascii_lowercase().as_str()) {
        (false, "actors") => RPGMFileType::Actors,
        (false, "armors") => RPGMFileType::Armors,
        (false, "classes") => RPGMFileType::Classes,
        (false, "commonevents") => RPGMFileType::Events,
        (false, "enemies") => RPGMFileType::Enemies,
        (false, "items") => RPGMFileType::Items,
        (false, "skills") => RPGMFileType::Skills,
        (false, "states") => RPGMFileType::States,
        (false, "troops") => RPGMFileType::Troops,
        (false, "weapons") => RPGMFileType::Weapons,
        (true, "actors") => RPGMFileType::Rm2kActors,
        (true, "skills") => RPGMFileType::Rm2kSkills,
        (true, "items") => RPGMFileType::Rm2kItems,
        (true, "enemies") => RPGMFileType::Rm2kEnemies,
        (true, "troops") => RPGMFileType::Rm2kTroops,
        (true, "classes") => RPGMFileType::Rm2kClasses,
        (true, "commonevents") => RPGMFileType::Rm2kCommonEvents,
        (true, "states") => RPGMFileType::Rm2kStates,
        (true, "terms") => RPGMFileType::Rm2kTerms,
        _ => {
            let expected = if engine_type == EngineType::RM2K {
                RM2K_SKIP_EVENT_NAMES
            } else {
                SKIP_EVENT_NAMES
            };

            return Err(format!(
                "`{name}` is not a file kind with events to skip for this engine. Expected one of: {expected}"
            ));
        }
    };

    Ok(file_type)
}

#[derive(Clone, Copy, Debug)]
pub struct FFlags(pub FileFlags);

impl FromStr for FFlags {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut flags = FileFlags::empty();

        for name in s.split(',').map(str::trim).filter(|name| !name.is_empty()) {
            flags.insert(resolve_file_kind(name)?);
        }

        Ok(Self(flags))
    }
}

const FILE_KIND_NAMES: &str = "map, actors, armors, classes, commonevents, enemies, items, skills, states, system, \
                               troops, weapons, scripts, plugins, database";

/// Resolves a `--skip-files` name, ignoring case. Names must match exactly: prefix matching would let a typo like
/// `actress` mean actors.
fn resolve_file_kind(name: &str) -> Result<FileFlags, String> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "actors" => FileFlags::Actors,
        "armors" => FileFlags::Armors,
        "classes" => FileFlags::Classes,
        "commonevents" => FileFlags::CommonEvents,
        "database" => FileFlags::Database,
        "enemies" => FileFlags::Enemies,
        "items" => FileFlags::Items,
        "map" => FileFlags::Map,
        "plugins" | "scripts" => FileFlags::Scripts,
        "skills" => FileFlags::Skills,
        "states" => FileFlags::States,
        "system" => FileFlags::System,
        "troops" => FileFlags::Troops,
        "weapons" => FileFlags::Weapons,
        _ => {
            return Err(format!(
                "`{name}` is not a file kind. Expected one of: {FILE_KIND_NAMES}"
            ));
        }
    })
}

/// Parses a codepage by its WHATWG label (`shift-jis`, `windows-1251`, ...).
///
/// Rejects non-ASCII-compatible codepages (UTF-16 and friends): decoding game text as one silently produces garbage.
/// Game files often carry no encoding, so the user has to name one. The `euc-kr` label is UHC/CP949.
fn parse_encoding(label: &str) -> Result<&'static Encoding, String> {
    let encoding = Encoding::for_label(label.as_bytes()).ok_or_else(|| format!("`{label}` is not a known encoding"))?;

    if !encoding.is_ascii_compatible() {
        return Err(format!(
            "`{label}` is not ASCII-compatible, so it can't be used for game text"
        ));
    }

    Ok(encoding)
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    duplicate_mode: DuplicateMode,
    line_separator: Option<String>,
    line_break: Option<String>,
    comment_prefix: Option<String>,
    /// Canonical WHATWG label, so it round-trips through [`Encoding::for_label`].
    /// Also needed on write: source lines are looked up in the `.txt`, which must be decoded the same way it was produced.
    read_encoding: Option<String>,
    /// Whether `read` recorded event headers. `append`/`purge` regenerate the files and would drop them otherwise.
    /// Absent in older metadata, meaning off.
    #[serde(default)]
    map_events: bool,
    #[serde(serialize_with = "serialize_sorted_hashes")]
    hashes: Option<HashMap<String, u64>>,
}

// serde's serialize_with hands the field over as &Option<T>, so the reference has to stay.
#[allow(clippy::ref_option)]
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

/// Resolves a codepage's canonical WHATWG label (as stored in `.rvpacker-metadata`, from [`Encoding::name`]) back to
/// an [`Encoding`], for [`Processor::read_encoding`]/[`Processor::write_encoding`].
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
    /// Skips processing specified file kinds, separated by comma. Names must match exactly, ignoring case:
    /// `map`, `actors`, `armors`, `classes`, `commonevents`, `enemies`, `items`, `skills`, `states`, `system`, `troops`,
    /// `weapons`, `scripts`, `database` (RM2K only). `plugins` is the same as `scripts`.
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

    /// Skips specified events. Has no effect on maps.
    /// Syntax: `file:0,1,..;file:0,1,..`. File names must match exactly, ignoring case: `actors`, `commonevents`, ...
    /// An unknown name is an error. RM2K takes its own set of names.
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
        value_parser = PossibleValuesParser::new(["allow", "remove"]).map(|s| DuplicateMode::from_str(&s).unwrap())
    )]
    duplicate_mode: Option<DuplicateMode>,
}

#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)]
struct ReadArgs {
    /// How to handle existing translation files.
    /// `default` - Leaves existing translation files alone, and writes only the missing ones.
    /// `append` - Adds new game text not already present, skips files unchanged since the last read, and keeps translations for text no longer in the game (see `--skip-obsolete`).
    /// `force` - Rewrites existing translation files, after asking for confirmation.
    /// `force-append` - `append` plus `force`, which bypasses the hash check.
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

    /// With `append`, drops translations for text no longer in the game, instead of keeping them.
    #[arg(long, alias = "so", action = ArgAction::SetTrue)]
    skip_obsolete: bool,

    /// Records where in the map events each line comes from. Recorded in `.rvpacker-metadata`, so `append`/`purge` keep it.
    #[arg(short, long, alias = "me", action = ArgAction::SetTrue)]
    map_events: bool,

    /// Prints `Game.ini`'s (or, if absent, `RPG_RT.ini`'s) title decoded with the given encoding, to help find `--read-encoding`.
    #[arg(long, value_name = "ENCODING", value_parser = parse_encoding)]
    probe_ini_title: Option<&'static Encoding>,

    /// Overrides the system file's title with the one decoded from `Game.ini`/`RPG_RT.ini` using `--read-encoding`.
    /// Opt-in: a wrong encoding silently corrupts the title.
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

    /// Keeps map event headers. Not needed if `read` used `--map-events`, which is recorded in `.rvpacker-metadata`.
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
    const fn extension(self) -> &'static str {
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

    /// JSON generation and writing commands
    Json {
        #[command(subcommand)]
        subcommand: JsonSubcommand,
    },

    /// Exports/imports translation files to/from structured formats
    /// (JSON, CSV, XML, XLSX, YAML)
    Serde {
        #[command(subcommand)]
        subcommand: SerdeSubcommand,
    },

    /// Decrypts/encrypts RPG Maker MV/MZ audio and image assets, without any game parsing. Uses `-i`/`-o` as directories
    Asset {
        #[command(flatten)]
        options: AssetOptions,

        #[command(subcommand)]
        command: AssetCommand,
    },

    /// Decrypts/encrypts RPG Maker XP/VX/VX Ace `.rgss` archives, without any game parsing. Uses `-i`/`-o` as paths
    Archive {
        #[command(subcommand)]
        subcommand: ArchiveCommand,
    },
}

/// Parses RPG Maker 2000/2003/XP/VX/VX Ace/MV/MZ game text to `.txt` files and writes them back. MV/MZ/XP/VX/VX Ace read
/// the `data`/`Data` directory and decrypt any `.rgss` archive; RPG Maker 2000/2003 read `RPG_RT.ldb` from the input directory.
/// Translation files go in `translation`.
#[derive(Debug, Parser)]
#[command(version = crate_version!(), next_line_help = true, term_width = 120)]
struct Cli {
    /// Input directory containing the game files. For MV/MZ, pass the `www` folder.
    #[arg(short, long, global = true, default_value = "./", value_name = "INPUT_PATH", value_parser = value_parser!(PathBuf), display_order = 1)]
    input_dir: PathBuf,

    /// Output directory to output files to
    #[arg(short, long, global = true, value_name = "OUTPUT_PATH", value_parser = value_parser!(PathBuf), display_order = 2)]
    output_dir: Option<PathBuf>,

    /// Separator between the source and translation columns. Defaults to `<#>`; unset uses the value in `.rvpacker-metadata`.
    #[arg(long, global = true, value_name = "SEPARATOR", display_order = 4)]
    line_separator: Option<String>,

    /// Marker that line breaks are normalized to in translation files. Defaults to `\#`; unset uses the value in `.rvpacker-metadata`.
    #[arg(long, global = true, value_name = "BREAK", display_order = 5)]
    line_break: Option<String>,

    /// Prefix marking a translation line as a comment (`ID`, `NAME`, ignore entries, ...). Defaults to `<!>`; unset uses the value in `.rvpacker-metadata`.
    #[arg(long, global = true, value_name = "PREFIX", display_order = 6)]
    comment_prefix: Option<String>,

    /// Codepage to decode the game's own text with. Needed for RPG Maker 2000/2003 and XP/VX `Scripts.*`, which store
    /// no encoding; unset and unrecorded, text is read as UTF-8 and text that isn't valid in it fails the run. VX Ace rarely needs it.
    ///
    /// Also used on write, to decode source lines when looking them up. `--probe-ini-title`/`--use-ini-title` help find
    /// the value: the `Game.ini` title is written in the same codepage as the rest of the game's text.
    ///
    /// Independent of `--write-encoding`. Unset, the value recorded in `.rvpacker-metadata` is used.
    #[arg(long, global = true, value_name = "ENCODING", value_parser = parse_encoding, display_order = 7)]
    read_encoding: Option<&'static Encoding>,

    /// Codepage to encode translated text with when writing. Unset writes UTF-8, which can't silently mangle a translation.
    ///
    /// Only set this if the target engine has no Unicode text rendering (RPG Maker 2000/2003, XP, VX), the translation
    /// fits the codepage, and the player's system uses it too. Unmappable characters become `&#NNNN;` references
    /// silently, so a mismatch doesn't fail loudly.
    ///
    /// Independent of `--read-encoding`. Never recorded in `.rvpacker-metadata`, so pass it on every `write`.
    #[arg(long, global = true, value_name = "ENCODING", value_parser = parse_encoding, display_order = 8)]
    write_encoding: Option<&'static Encoding>,

    /// Codepage the file names inside a `.rgss` archive decode with, when `read` or `json generate` extracts it. Unset,
    /// `--read-encoding` is used.
    ///
    /// Not recorded in `.rvpacker-metadata`, as the archive is extracted once, when the system file is missing.
    #[arg(long, global = true, value_name = "ENCODING", value_parser = parse_encoding, display_order = 9)]
    archive_read_encoding: Option<&'static Encoding>,

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
    archive_read_encoding: Option<&'static Encoding>,

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

        // 2000/2003 keep their files at the project root with no `data`/`Data` directory, so check `RPG_RT.ldb` first.
        let rm2k_database_path = input_dir.join("RPG_RT.ldb");

        let (engine_type, source_path, system_file_path, archive_path) = if rm2k_database_path.exists() {
            (EngineType::RM2K, input_dir.clone(), rm2k_database_path, None)
        } else {
            // Archive-only games get their data directory when `read` extracts the archive.
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
        let read_encoding = take(&mut cli.read_encoding).map(|e| e.name().to_string());
        let write_encoding = take(&mut cli.write_encoding).map(|e| e.name().to_string());
        let archive_read_encoding = take(&mut cli.archive_read_encoding);

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
            archive_read_encoding,
            start_time,
        })
    }

    /// Reads the raw title bytes from `Game.ini`, or from `RPG_RT.ini` if `Game.ini` is absent.
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
    fn print_ini_title(&self, encoding: &'static Encoding) -> Result<(), anyhow::Error> {
        let title_bytes = self.read_ini_title()?;
        let (title, _, had_errors) = encoding.decode(&title_bytes);

        println!("{title}");

        if had_errors {
            eprintln!("warning: decoding produced replacement characters - this is probably not the right encoding");
        }

        Ok(())
    }

    /// Turns the `--skip-events` names into the file kinds the library keys its skip lists by.
    /// Fails on the first unknown name.
    fn skip_event_kinds(&self, skip_events: SkipEvents) -> Result<Vec<(RPGMFileType, Vec<u32>)>, anyhow::Error> {
        skip_events
            .0
            .into_iter()
            .map(|(name, ids)| {
                let file_type = resolve_file_type(&name, self.engine_type).map_err(|error| anyhow!(error))?;
                Ok((file_type, ids))
            })
            .collect()
    }

    /// Whether a setting that changes the translation text was passed on the command line with a value other than the
    /// recorded one.
    fn text_settings_changed(&self, cli_map_events: bool, recorded: &Metadata) -> bool {
        let differs =
            |cli: Option<&String>, recorded: &Option<String>| cli.is_some_and(|cli| recorded.as_ref() != Some(cli));

        differs(self.line_separator.as_ref(), &recorded.line_separator)
            || differs(self.line_break.as_ref(), &recorded.line_break)
            || differs(self.comment_prefix.as_ref(), &recorded.comment_prefix)
            || differs(self.read_encoding.as_ref(), &recorded.read_encoding)
            || (cli_map_events && !recorded.map_events)
    }

    /// Precedence: explicit command-line flag, then `.rvpacker-metadata`, then defaults. Recorded format settings win
    /// over anything the user didn't ask for. A recorded `hashes` passes through untouched.
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

    /// Extracts the `.rgss` archive into the input directory unless the system file already exists. Names decode with
    /// `--archive-read-encoding`, else `read_encoding` (UTF-8 if neither is set). Undecodable entries are skipped with a
    /// warning rather than written under a colliding name.
    fn extract_archive(&self, read_encoding: Option<&'static Encoding>) -> Result<(), anyhow::Error> {
        let Some(archive_path) = &self.archive_path else {
            return Ok(());
        };

        if self.system_file_path.exists() {
            return Ok(());
        }

        let encoding = self.archive_read_encoding.or(read_encoding).unwrap_or(UTF_8);
        // Entries are decrypted one at a time as they're read, so the archive itself never has to fit in memory.
        let mut archive = decrypt_from_reader(BufReader::new(
            File::open(archive_path).with_context(|| archive_path.display().to_string())?,
        ))?;
        let mut skipped: usize = 0;

        for index in 0..archive.entry_count() {
            let raw_path = archive.path(index);
            let Some(decoded) = encoding.decode_without_bom_handling_and_without_replacement(raw_path) else {
                eprintln!(
                    "warning: skipped an archive entry, its name isn't valid {}: {}",
                    encoding.name(),
                    String::from_utf8_lossy(raw_path)
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

            // `path` and `raw_path` borrow the archive, so they must be dead before the entry is read below.
            let mut entry = archive.read_entry(index)?;
            let mut output = File::create(&output_file_path).with_context(|| output_file_path.display().to_string())?;
            copy(&mut entry, &mut output).with_context(|| output_file_path.display().to_string())?;
        }

        if skipped > 0 {
            eprintln!(
                "warning: {skipped} archive entries were skipped. If they matter, pass the right codepage to \
                 `--archive-read-encoding` and run again."
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
        // Resolved before anything is prompted or written, so a typo fails the run without side effects.
        let skip_events = self.skip_event_kinds(skip_events)?;

        let Mode::Read { append, force } = read_mode else {
            unreachable!("the read-mode parser only accepts `Read` variants")
        };

        // Without `append`, metadata only supplies settings, so a broken file must not block a re-read that replaces it.
        let recorded = match parse_metadata(&self.metadata_file_path) {
            Ok(recorded) => recorded,
            Err(error) if !append => {
                eprintln!("warning: ignoring the unreadable `.rvpacker-metadata`: {error:#}");
                None
            }
            Err(error) => return Err(error),
        };

        let text_settings_changed = recorded
            .as_ref()
            .is_some_and(|recorded| self.text_settings_changed(cli_map_events, recorded));

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

        // Hashes decide which files are skipped as unchanged, so only `append` may reuse the recorded ones, and not
        // when a text setting changed: hashes cover only the game files' bytes, so unchanged files would keep the old
        // decoding or formatting.
        let hashes = if append && !text_settings_changed {
            hashes.unwrap_or_default()
        } else {
            HashMap::default()
        };

        if force && !silent {
            let start = Instant::now();
            println!("WARNING! Force mode will forcefully rewrite all your translation files. Input 'Y' to continue.");

            let mut buf = String::with_capacity(4);
            stdin().read_line(&mut buf)?;

            // Non-zero exit, so a script can't mistake a read that never ran for a successful one.
            if buf.trim_end() != "Y" {
                bail!("Aborted.");
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
            skip_events,
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
        let skip_events = self.skip_event_kinds(skip_events)?;
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
            skip_events,
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
        let skip_events = self.skip_event_kinds(skip_events)?;
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
        } = self.merge_metadata(
            duplicate_mode,
            args.map_events,
            parse_metadata(&self.metadata_file_path)?,
        );

        apply_format_overrides(line_separator.as_ref(), line_break.as_ref(), comment_prefix.as_ref());

        let mut flags = BaseFlags::empty();
        flags.set(BaseFlags::CreateIgnore, create_ignore);

        let mut processor = Processor {
            mode: Mode::Purge,
            file_flags,
            flags,
            duplicate_mode,
            skip_maps: skip_maps.0,
            skip_events,
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
                let read_encoding = resolve_encoding(self.read_encoding.as_deref());
                self.extract_archive(read_encoding)?;
                generate(&self.source_path, &json_path, force, read_encoding)?;
            }
            JsonSubcommand::Write => {
                let write_encoding = resolve_encoding(self.write_encoding.as_deref());
                write(json_path, json_output_path, self.engine_type, write_encoding)?;
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

                for entry in read_dir(&self.translation_path)? {
                    let path = entry?.path();

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

                for entry in read_dir(&serde_path)? {
                    let path = entry?.path();

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

    // These commands don't read a game, so they run before `Session` looks for one in the input directory.
    if let Command::Asset { options, command } = &cli.command {
        execute_asset(options, *command, &cli.input_dir, cli.output_dir.as_deref())?;
        println!("Elapsed: {:.2}s", start_time.elapsed().as_secs_f32());
        return Ok(());
    }

    if let Command::Archive { subcommand } = &cli.command {
        execute_archive(subcommand, &cli.input_dir, cli.output_dir.as_deref())?;
        println!("Elapsed: {:.2}s", start_time.elapsed().as_secs_f32());
        return Ok(());
    }

    let mut session = Session::new(&mut cli, &mut start_time)?;

    match cli.command {
        Command::Read(args) => session.execute_read(args)?,
        Command::Write(args) => session.execute_write(args)?,
        Command::Purge(args) => session.execute_purge(args)?,
        Command::Json { subcommand } => session.execute_json(&subcommand)?,
        Command::Serde { subcommand } => session.execute_serde(&subcommand)?,
        Command::Asset { .. } | Command::Archive { .. } => unreachable!("handled before `Session` is created"),
    }

    println!("Elapsed: {:.2}s", start_time.elapsed().as_secs_f32());
    Ok(())
}
