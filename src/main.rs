#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::needless_doctest_main)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::deref_addrof)]

use anyhow::{Context, Result, bail};
use clap::{
    ArgAction, Args, Parser, Subcommand, ValueEnum, crate_version, value_parser,
};
use rpgmad_lib::Decrypter;
use rvpacker_lib::{
    BaseFlags, PurgerBuilder, RVPACKER_IGNORE_FILE, RVPACKER_METADATA_FILE,
    ReaderBuilder, WriterBuilder, get_ini_title, get_system_title, json,
    types::{EngineType, FileFlags, GameType},
};
use serde::{Deserialize, Serialize};
use serde_json::{from_str, to_string};
use std::{
    fs::{create_dir_all, read, read_to_string, write},
    io::stdin,
    mem::transmute,
    path::{Path, PathBuf},
    process::exit,
    time::Instant,
};
use strum_macros::EnumIs;

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    romanize: bool,
    disable_custom_processing: bool,
    trim: bool,
    duplicate_mode: rvpacker_lib::DuplicateMode,
}

#[derive(Debug, Copy, Clone, ValueEnum)]
enum DisableProcessing {
    Maps = 1,
    Other = 2,
    System = 4,
    Scripts = 8,
}

#[derive(Debug, Copy, Clone, ValueEnum)]
enum Engine {
    MV,
    MZ,
}

#[derive(Default, Debug, Copy, Clone, ValueEnum)]
pub enum ReadMode {
    #[default]
    Default,
    Append,
    Force,
}

#[derive(Default, Debug, Copy, Clone, ValueEnum)]
pub enum DuplicateMode {
    #[default]
    Allow,
    Remove,
}

/// This tool allows to parse RPG Maker XP/VX/VXAce/MV/MZ games text to `.txt` files and write them back to their initial form. The program uses `original` or `data` directories for source files, and `translation` directory to operate with translation files. It will also decrypt any `.rgss` archive if it's present.
#[derive(Parser, Debug)]
#[command(name = "", version = crate_version!(), next_line_help = true, term_width = 120)]
struct Cli {
    /// Input directory, containing game files
    #[arg(short, long, global = true, default_value = "./", value_name = "INPUT_PATH", value_parser = value_parser!(PathBuf), display_order = 1)]
    input_dir: PathBuf,

    /// Output directory to output files to
    #[arg(short, long, global = true, value_name = "OUTPUT_PATH", value_parser = value_parser!(PathBuf), display_order = 2)]
    output_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,

    #[command(flatten)]
    verbosity: clap_verbosity_flag::Verbosity,
}

#[derive(Debug, Subcommand, EnumIs)]
pub enum Command {
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
}

#[derive(Debug, Args)]
pub struct ReadArgs {
    #[arg(short, long, hide = true, action = ArgAction::SetTrue)]
    silent: bool,

    /// Ignore entries from `.rvpacker-ignore` file. Use with `--mode append`
    #[arg(short = 'I', long, action = ArgAction::SetTrue)]
    ignore: bool,

    #[command(flatten)]
    shared: SharedArgs,
}

#[derive(Debug, Args)]
pub struct SharedArgs {
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
        display_order = 3
    )]
    read_mode: ReadMode,

    /// Removes the leading and trailing whitespace from extracted strings. Don't use this option unless you know that trimming the text won't cause any incorrect behavior
    #[arg(short, long, action = ArgAction::SetTrue, display_order = 6)]
    trim: bool,

    /// If you parsing text from a Japanese game, that contains symbols like 「」, which are just the Japanese quotation marks, it automatically replaces these symbols by their western equivalents (in this case, '').
    /// Will be automatically set if it was used in read
    #[arg(short = 'R', long, action = ArgAction::SetTrue, display_order = 5)]
    romanize: bool,

    /// Disables built-in custom processing, implemented for some games.
    /// Right now, implemented for the following titles: LISA: The Painful and its derivatives, Fear & Hunger 2: Termina.
    /// Will be automatically set if it was used in read.
    #[arg(short = 'D', long, alias = "no-custom", action = ArgAction::SetTrue, display_order = 93)]
    disable_custom_processing: bool,

    /// Skips processing specified files. plugins can be used interchangeably with scripts
    #[arg(
        long,
        alias = "no",
        value_delimiter = ',',
        value_name = "FILES",
        display_order = 94
    )]
    disable_processing: Vec<DisableProcessing>,

    /// Controls how to handle duplicates in text
    #[arg(
        short,
        long,
        alias = "dup-mode",
        default_value = "remove",
        display_order = 93
    )]
    duplicate_mode: DuplicateMode,
}

#[derive(Debug, Args)]
pub struct PurgeArgs {
    /// Creates an ignore file from purged lines, to prevent their further appearance when reading with `--mode append`
    #[arg(short, long, action = ArgAction::SetTrue, display_order = 23)]
    pub create_ignore: bool,

    #[command(flatten)]
    pub shared: SharedArgs,
}

#[derive(Debug, Subcommand)]
pub enum JsonSubcommand {
    /// Generates JSON representations of older engines' files in `json` directory
    Generate {
        #[arg(
            short,
            long,
            alias = "mode",
            default_value = "default",
            value_name = "MODE"
        )]
        read_mode: ReadMode,
    },

    /// Writes JSON representations of older engines' files from `json` directory back to original files
    Write,
}

fn parse_metadata(metadata_file_path: &Path) -> Result<Option<Metadata>> {
    if !metadata_file_path.exists() {
        return Ok(None);
    }

    let metadata_file_content = read_to_string(metadata_file_path)?;
    let metadata = from_str(&metadata_file_content)?;
    Ok(Some(metadata))
}

fn get_game_type(
    game_title: &str,
    disable_custom_processing: bool,
) -> GameType {
    if disable_custom_processing {
        GameType::None
    } else {
        let lowercased = game_title.to_lowercase();

        if lowercased.contains("termina") {
            GameType::Termina
        } else if lowercased.contains("lisa") {
            GameType::LisaRPG
        } else {
            GameType::None
        }
    }
}

struct Processor<'a> {
    engine_type: EngineType,

    input_dir: PathBuf,
    system_file_path: PathBuf,
    ini_file_path: PathBuf,
    metadata_file_path: PathBuf,

    source_path: PathBuf,
    translation_path: PathBuf,
    ignore_file_path: PathBuf,

    archive_path: Option<PathBuf>,
    output_dir: PathBuf,

    start_time: &'a mut Instant,
}

impl<'a> Processor<'a> {
    pub fn new(
        cli: &mut Cli,
        start_time: &'a mut Instant,
    ) -> Result<Self, anyhow::Error> {
        let input_dir = std::mem::take(&mut cli.input_dir);

        if !input_dir.exists() {
            bail!("Input directory does not exist.");
        }

        let output_dir = std::mem::take(&mut cli.output_dir)
            .unwrap_or_else(|| input_dir.clone());

        if !output_dir.exists() {
            bail!("Output directory does not exist.");
        }

        let source_path = ["original", "data", "Data"]
            .into_iter()
            .find_map(|dir| {
                let path = input_dir.join(dir);

                if path.exists() {
                    return Some(path);
                }

                None
            })
            .context(
                "Could not found `original` or `data`/`Data` directory.",
            )?;

        let translation_path = output_dir.join("translation");
        let metadata_file_path = translation_path.join(RVPACKER_METADATA_FILE);
        let ignore_file_path = translation_path.join(RVPACKER_IGNORE_FILE);

        let type_paths = [
            (EngineType::New, source_path.join("System.json"), None),
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

        let Some((engine_type, system_file_path, archive_path)) = type_paths
            .into_iter()
            .find_map(|(engine_type, system_file_path, archive_path)| {
                if !system_file_path.exists()
                    && archive_path.as_ref().is_none_or(|path| !path.exists())
                {
                    return None;
                }

                Some((engine_type, system_file_path, archive_path))
            })
        else {
            bail!(
                "Couldn't determine game engine. Check the existence of `System` file inside `original` or `data`/`Data` directory, or `.rgss` archive."
            );
        };

        let ini_file_path = input_dir.join("Game.ini");

        Ok(Self {
            engine_type,
            input_dir,
            system_file_path,
            ini_file_path,
            metadata_file_path,
            source_path,
            translation_path,
            ignore_file_path,
            archive_path,
            output_dir,
            start_time,
        })
    }

    fn get_game_title(&self) -> Result<String> {
        Ok(if self.engine_type.is_new() {
            get_system_title(&read_to_string(&self.system_file_path)?)?
        } else {
            String::from_utf8_lossy(&get_ini_title(&read(
                &self.ini_file_path,
            )?)?)
            .into_owned()
        })
    }

    pub fn execute_read(
        &mut self,
        args: ReadArgs,
    ) -> Result<(), anyhow::Error> {
        let mut file_flags = FileFlags::all();
        let mut romanize = args.shared.romanize;
        let mut trim = args.shared.trim;
        let mut duplicate_mode: rvpacker_lib::DuplicateMode =
            unsafe { transmute(args.shared.duplicate_mode) };
        let read_mode: rvpacker_lib::ReadMode =
            unsafe { transmute(args.shared.read_mode) };
        let silent = args.silent;
        let ignore = args.ignore;
        let mut disable_custom_processing =
            args.shared.disable_custom_processing;

        for arg in args.shared.disable_processing {
            file_flags.remove(FileFlags::from_bits_truncate(arg as u8));
        }

        let game_title = self.get_game_title()?;

        let game_type = get_game_type(&game_title, disable_custom_processing);

        if read_mode.is_append()
            && let Some(metadata) = parse_metadata(&self.metadata_file_path)?
        {
            Metadata {
                romanize,
                trim,
                duplicate_mode,
                disable_custom_processing,
            } = metadata;
        }

        if read_mode.is_force() && !silent {
            let start = Instant::now();
            println!(
                "WARNING! Force mode will forcefully rewrite all your translation files. Input 'Y' to continue."
            );

            let mut buf = String::with_capacity(4);
            stdin().read_line(&mut buf)?;

            if buf.trim_end() != "Y" {
                exit(0);
            }

            *self.start_time -= start.elapsed();
        }

        if !read_mode.is_append() {
            let metadata = Metadata {
                romanize,
                disable_custom_processing,
                trim,
                duplicate_mode,
            };

            create_dir_all(&self.translation_path)?;
            write(&self.metadata_file_path, to_string(&metadata)?)?;
        } else if ignore && !self.ignore_file_path.exists() {
            bail!(
                "`.rvpacker-ignore` file does not exist. Aborting execution."
            );
        }

        if let Some(archive_path) = &self.archive_path
            && !self.system_file_path.exists()
        {
            let archive_data = read(archive_path)?;
            let decrypted_files = Decrypter::new().decrypt(&archive_data)?;

            for file in decrypted_files {
                let path = String::from_utf8_lossy(&file.path);
                let output_file_path = self.input_dir.join(path.as_ref());

                if let Some(parent) = output_file_path.parent() {
                    create_dir_all(parent)?;
                }

                write(output_file_path, file.data)?;
            }
        }

        let mut flags = BaseFlags::empty();
        flags.set(BaseFlags::Romanize, romanize);
        flags.set(BaseFlags::Ignore, ignore);
        flags.set(BaseFlags::Trim, trim);

        ReaderBuilder::new()
            .with_files(file_flags)
            .with_flags(flags)
            .game_type(game_type)
            .read_mode(read_mode)
            .duplicate_mode(duplicate_mode)
            .build()
            .read(
                &self.source_path,
                &self.translation_path,
                self.engine_type,
            )?;

        Ok(())
    }

    pub fn execute_write(&self, args: SharedArgs) -> Result<(), anyhow::Error> {
        if !self.translation_path.exists() {
            bail!(
                "`translation` directory in the input directory does not exist."
            );
        }

        let mut file_flags = FileFlags::all();
        let mut romanize = args.romanize;
        let mut trim = args.trim;
        let mut duplicate_mode: rvpacker_lib::DuplicateMode =
            unsafe { transmute(args.duplicate_mode) };
        let mut disable_custom_processing = args.disable_custom_processing;

        for arg in args.disable_processing {
            file_flags.remove(FileFlags::from_bits_truncate(arg as u8));
        }

        if let Some(metadata) = parse_metadata(&self.metadata_file_path)? {
            Metadata {
                romanize,
                trim,
                duplicate_mode,
                disable_custom_processing,
            } = metadata;
        }

        let game_title = self.get_game_title()?;

        let game_type = get_game_type(&game_title, disable_custom_processing);

        let mut flags = BaseFlags::empty();
        flags.set(BaseFlags::Romanize, romanize);
        flags.set(BaseFlags::Trim, trim);

        WriterBuilder::new()
            .with_files(file_flags)
            .with_flags(flags)
            .game_type(game_type)
            .duplicate_mode(duplicate_mode)
            .build()
            .write(
                &self.source_path,
                &self.translation_path,
                &self.output_dir.join("output"),
                self.engine_type,
            )?;

        Ok(())
    }

    pub fn execute_purge(&self, args: PurgeArgs) -> Result<(), anyhow::Error> {
        let mut file_flags = FileFlags::all();
        let create_ignore = args.create_ignore;
        let mut romanize = args.shared.romanize;
        let mut trim = args.shared.trim;
        let mut duplicate_mode: rvpacker_lib::DuplicateMode =
            unsafe { transmute(args.shared.duplicate_mode) };
        let mut disable_custom_processing =
            args.shared.disable_custom_processing;

        for arg in args.shared.disable_processing {
            file_flags.remove(FileFlags::from_bits_truncate(arg as u8));
        }

        if let Some(metadata) = parse_metadata(&self.metadata_file_path)? {
            Metadata {
                romanize,
                trim,
                duplicate_mode,
                disable_custom_processing,
            } = metadata;
        }

        let game_title = self.get_game_title()?;

        let game_type = get_game_type(&game_title, disable_custom_processing);

        let mut flags: BaseFlags = BaseFlags::empty();
        flags.set(BaseFlags::Romanize, romanize);
        flags.set(BaseFlags::Trim, trim);
        flags.set(BaseFlags::CreateIgnore, create_ignore);

        PurgerBuilder::new()
            .with_files(file_flags)
            .with_flags(flags)
            .game_type(game_type)
            .duplicate_mode(duplicate_mode)
            .build()
            .purge(
                &self.source_path,
                &self.translation_path,
                self.engine_type,
            )?;

        Ok(())
    }

    pub fn execute_json(
        &self,
        subcommand: &JsonSubcommand,
    ) -> Result<(), anyhow::Error> {
        use json::{generate, write};

        let json_path = self.input_dir.join("json");
        let json_output_path = self.input_dir.join("json-output");

        match subcommand {
            JsonSubcommand::Generate { read_mode } => {
                let read_mode: rvpacker_lib::ReadMode =
                    unsafe { transmute(*read_mode) };

                generate(&self.source_path, &json_path, read_mode.is_force())?;
            }
            JsonSubcommand::Write => {
                write(json_path, json_output_path, self.engine_type)?;
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

    let mut processor = Processor::new(&mut cli, &mut start_time)?;

    match cli.command {
        Command::Read(args) => processor.execute_read(args)?,
        Command::Write(args) => processor.execute_write(args)?,
        Command::Purge(args) => processor.execute_purge(args)?,
        Command::Json { subcommand } => processor.execute_json(&subcommand)?,
    }

    println!("Elapsed: {:.2}s", start_time.elapsed().as_secs_f32());
    Ok(())
}
