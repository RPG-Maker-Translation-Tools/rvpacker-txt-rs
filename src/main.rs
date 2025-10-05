use anyhow::{Context, Result, bail};
use clap::{
    ArgAction, Args, Parser, Subcommand, ValueEnum, crate_version, value_parser,
};
use rpgmad_lib::Decrypter;
use rvpacker_lib::{
    PurgerBuilder, RVPACKER_IGNORE_FILE, RVPACKER_METADATA_FILE, ReaderBuilder,
    WriterBuilder, get_ini_title, get_system_title, json, types::*,
};
use serde::{Deserialize, Serialize};
use serde_json::{from_str, to_string};
use std::{
    ffi::OsStr,
    fs::{create_dir_all, read, read_dir, read_to_string, write},
    io::stdin,
    mem::transmute,
    path::PathBuf,
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

    /// Decrypt/encrypt RPG Maker MV/MZ audio and image assets
    Asset(AssetArgs),
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

#[derive(Debug, Args)]
pub struct AssetArgs {
    #[command(subcommand)]
    subcommand: AssetSubcommand,

    /// Encryption key for encrypt/decrypt operations
    #[arg(short, long, value_name = "KEY")]
    key: Option<String>,

    /// File path (for single file processing or key extraction)
    #[arg(short, long, value_name = "INPUT_FILE", value_parser = value_parser!(PathBuf))]
    file: Option<PathBuf>,

    /// Game engine (`mv` or `mz`)
    #[arg(short, long, value_name = "ENGINE")]
    engine: Option<Engine>,
}

#[derive(Debug, Subcommand, EnumIs)]
pub enum AssetSubcommand {
    /// Decrypts encrypted assets.
    /// `.rpgmvo`/`.ogg_` => `.ogg`
    /// `.rpgmvp`/`.png_` => `.png`
    /// `.rpgmvm`/`.m4a_` => `.m4a`
    Decrypt,

    /// Encrypts `.png`/`.ogg`/`.m4a` assets.
    /// `.ogg` => `.rpgmvo`/`.ogg_`
    /// `.png` => `.rpgmvp`/`.png_`
    /// `.m4a` => `.rpgmvm`/`.m4a_`
    Encrypt,

    /// Extracts key from the file, specified in `--file` argument
    ExtractKey,
}

fn parse_metadata(metadata_file_path: &PathBuf) -> Result<Option<Metadata>> {
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
) -> Result<GameType> {
    Ok(if disable_custom_processing {
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
    })
}

fn main() -> Result<()> {
    let mut start_time = Instant::now();
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .without_time()
        .with_target(false)
        .with_level(true)
        .with_thread_names(false)
        .with_thread_ids(false)
        .with_ansi(true)
        .with_max_level(cli.verbosity)
        .init();

    let input_dir = cli.input_dir;

    if !input_dir.exists() {
        bail!("Input directory does not exist.");
    }

    let output_dir = cli.output_dir.unwrap_or_else(|| input_dir.clone());

    if !output_dir.exists() {
        bail!("Output directory does not exist.")
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
        .context("Could not found `original` or `data`/`Data` directory.")?;

    let translation_path = &output_dir.join("translation");
    let metadata_file_path = &translation_path.join(RVPACKER_METADATA_FILE);
    let ignore_file_path = &translation_path.join(RVPACKER_IGNORE_FILE);

    let Some((engine_type, system_file_path, archive_path)) = [
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
    ]
    .into_iter()
    .find_map(|(engine_type, system_file_path, archive_path)| {
        if !system_file_path.exists()
            && archive_path.as_ref().is_none_or(|path| !path.exists())
        {
            return None;
        }

        Some((engine_type, system_file_path, archive_path))
    }) else {
        bail!(
            "Couldn't determine game engine. Check the existence of `System` file inside `original` or `data`/`Data` directory, or `.rgss` archive."
        );
    };

    let ini_file_path = input_dir.join("Game.ini");

    let get_game_title = || -> Result<String> {
        Ok(if engine_type.is_new() {
            get_system_title(&read_to_string(&system_file_path)?)?
        } else {
            String::from_utf8_lossy(&get_ini_title(&read(ini_file_path)?)?)
                .into_owned()
        })
    };

    match cli.command {
        Command::Read(args) => {
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

            let game_title = get_game_title()?;

            let game_type =
                get_game_type(&game_title, disable_custom_processing)?;

            if read_mode.is_append() {
                if let Some(metadata) = parse_metadata(metadata_file_path)? {
                    Metadata {
                        romanize,
                        trim,
                        duplicate_mode,
                        disable_custom_processing,
                    } = metadata;
                }
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

                start_time -= start.elapsed();
            }

            if !read_mode.is_append() {
                let metadata = Metadata {
                    romanize,
                    disable_custom_processing,
                    trim,
                    duplicate_mode,
                };

                create_dir_all(translation_path)?;
                write(metadata_file_path, to_string(&metadata)?)?;
            } else if ignore && !ignore_file_path.exists() {
                bail!(
                    "`.rvpacker-ignore` file does not exist. Aborting execution."
                );
            }

            if let Some(archive_path) = archive_path {
                if !system_file_path.exists() {
                    let archive_data = read(archive_path)?;
                    let decrypted_files =
                        Decrypter::new().decrypt(&archive_data)?;

                    for file in decrypted_files {
                        let path = String::from_utf8_lossy(&file.path);
                        let output_file_path = input_dir.join(path.as_ref());

                        if let Some(parent) = output_file_path.parent() {
                            create_dir_all(parent)?;
                        }

                        write(output_file_path, file.content)?;
                    }
                }
            }

            ReaderBuilder::new()
                .with_flags(file_flags)
                .romanize(romanize)
                .game_type(game_type)
                .read_mode(read_mode)
                .ignore(ignore)
                .trim(trim)
                .duplicate_mode(duplicate_mode)
                .build()
                .read(&source_path, translation_path, engine_type)?;
        }

        Command::Write(args) => {
            if !translation_path.exists() {
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

            if let Some(metadata) = parse_metadata(metadata_file_path)? {
                Metadata {
                    romanize,
                    trim,
                    duplicate_mode,
                    disable_custom_processing,
                } = metadata;
            }

            let game_title = get_game_title()?;

            let game_type =
                get_game_type(&game_title, disable_custom_processing)?;

            WriterBuilder::new()
                .with_flags(file_flags)
                .romanize(romanize)
                .game_type(game_type)
                .trim(trim)
                .duplicate_mode(duplicate_mode)
                .build()
                .write(
                    &source_path,
                    translation_path,
                    &output_dir.join("output"),
                    engine_type,
                )?;
        }

        Command::Purge(args) => {
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

            if let Some(metadata) = parse_metadata(metadata_file_path)? {
                Metadata {
                    romanize,
                    trim,
                    duplicate_mode,
                    disable_custom_processing,
                } = metadata;
            }

            let game_title = get_game_title()?;

            let game_type =
                get_game_type(&game_title, disable_custom_processing)?;

            PurgerBuilder::new()
                .with_flags(file_flags)
                .romanize(romanize)
                .game_type(game_type)
                .trim(trim)
                .duplicate_mode(duplicate_mode)
                .create_ignore(create_ignore)
                .build()
                .purge(&source_path, translation_path, engine_type)?;
        }

        Command::Json { subcommand } => {
            use json::*;

            let json_path = input_dir.join("json");
            let json_output_path = input_dir.join("json-output");

            match subcommand {
                JsonSubcommand::Generate { read_mode } => {
                    let read_mode: rvpacker_lib::ReadMode =
                        unsafe { transmute(read_mode) };

                    generate(&source_path, &json_path, read_mode.is_force())?;
                }
                JsonSubcommand::Write => {
                    write(json_path, json_output_path, engine_type)?;
                }
            }
        }

        Command::Asset(args) => {
            use asset_decrypter::*;

            let key = args.key;
            let file = args.file;
            let engine =
                args.engine.context("`--engine` argument is required.")?;

            if let Some(ref file) = file {
                if !file.is_file() {
                    bail!(
                        "`--file` argument is missing. It's required in `extract_key` command."
                    );
                }
            } else if args.subcommand.is_extract_key() {
                bail!("`--file` argument expects a file.")
            }

            let mut decrypter = Decrypter::new();

            if key.is_none() && args.subcommand.is_encrypt() {
                decrypter.set_key_from_str(DEFAULT_KEY)?
            } else {
                decrypter
                    .set_key_from_str(&unsafe { key.unwrap_unchecked() })?
            };

            if args.subcommand.is_extract_key() {
                let file = unsafe { file.unwrap_unchecked() };
                let filename = unsafe { file.file_name().unwrap_unchecked() };
                let extension = unsafe { file.extension().unwrap_unchecked() };

                let content: String;

                let key = if filename == "System.json" {
                    content = read_to_string(file)?;
                    let index = unsafe {
                        content.rfind("encryptionKey").unwrap_unchecked()
                    } + "encryptionKey\":".len();
                    &content[index..].trim().trim_matches('"')[..KEY_LENGTH]
                } else if extension == "png_" || extension == "rpgmvp" {
                    let buf = read(file)?;

                    decrypter.set_key_from_image(&buf);
                    unsafe { decrypter.key().unwrap_unchecked() }
                } else {
                    bail!(
                        "Key can be extracted only from `System.json` or `.png_`/`.rpgmvp` file."
                    );
                };

                println!("Encryption key: {key}");
            } else {
                let mut process_file = |path: &PathBuf,
                                        filename: &OsStr,
                                        extension: &str|
                 -> Result<()> {
                    let data = read(path)?;

                    let (processed, new_ext) = if args.subcommand.is_decrypt() {
                        let decrypted = decrypter.decrypt(&data);
                        let new_ext = match extension {
                            "rpgmvp" | "png_" => "png",
                            "rpgmvo" | "ogg_" => "ogg",
                            "rpgmvm" | "m4a_" => "m4a",
                            _ => unreachable!(),
                        };
                        (decrypted, new_ext)
                    } else {
                        let encrypted = decrypter.encrypt(&data)?;
                        let new_ext = match (engine, extension) {
                            (Engine::MV, "png") => "rpgmvp",
                            (Engine::MV, "ogg") => "rpgmvo",
                            (Engine::MV, "m4a") => "rpgmvm",
                            (Engine::MZ, "png") => "png_",
                            (Engine::MZ, "ogg") => "ogg_",
                            (Engine::MZ, "m4a") => "m4a_",
                            _ => unreachable!(),
                        };
                        (encrypted, new_ext)
                    };

                    let output_file = output_dir
                        .join(PathBuf::from(filename).with_extension(new_ext));
                    write(output_file, processed)?;

                    Ok(())
                };

                let exts: &[&str] = if args.subcommand.is_encrypt() {
                    &["png", "ogg", "m4a"]
                } else {
                    &["rpgmvp", "rpgmvo", "rpgmvm", "ogg_", "png_", "m4a_"]
                };

                if let Some(file) = &file {
                    let filename =
                        unsafe { file.file_name().unwrap_unchecked() };
                    let extension = unsafe {
                        file.extension()
                            .and_then(|ext| ext.to_str())
                            .unwrap_unchecked()
                    };

                    if exts.contains(&extension) {
                        process_file(file, filename, extension)?;
                    }
                } else {
                    for entry in read_dir(input_dir)?.flatten() {
                        let path = entry.path();
                        let filename = entry.file_name();
                        let extension =
                            match path.extension().and_then(|ext| ext.to_str())
                            {
                                Some(ext) => ext,
                                None => continue,
                            };

                        if exts.contains(&extension) {
                            process_file(&path, &filename, extension)?;
                        }
                    }
                }
            }
        }
    }

    println!("Elapsed: {:.2}s", start_time.elapsed().as_secs_f32());
    Ok(())
}
