use anyhow::{Context, Result, bail};
use asset_decrypter::{
    DECRYPTED_ASSET_EXTS, Decrypter, ENCRYPTED_ASSET_EXTS, FileType, HEADER_LENGTH, M4A_EXT, OGG_EXT, PNG_EXT,
    RPGM_HEADER,
};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, from_str};
use std::{
    fs::{File, create_dir_all, read, read_dir, read_to_string, write},
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum AssetEngine {
    MV,
    MZ,
}

/// Options shared by every `asset` subcommand. Input and output directories come from the top-level `-i`/`-o`.
#[derive(Args, Debug)]
pub struct AssetOptions {
    /// Encryption key. Not needed for decrypting, which reads the key from the files
    #[arg(short = 'e', long, global = true, value_name = "KEY")]
    key: Option<String>,

    /// Game engine - `mv` or `mz`. Required for encryption
    #[arg(short = 'E', long, global = true, value_name = "ENGINE")]
    engine: Option<AssetEngine>,

    /// File path (for single file processing or key extraction). Its directory is the output unless `-o` is set
    #[arg(short, long, global = true, value_name = "FILE")]
    file: Option<PathBuf>,

    /// Decrypt: overwrite output files that already exist. Without it, existing files are skipped
    #[arg(long, global = true)]
    force: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Subcommand)]
pub enum AssetCommand {
    /// Encrypts .png/.ogg/.m4a assets. Requires `--engine` and `--key`
    ///
    /// .ogg => .rpgmvo/.ogg_
    ///
    /// .png => .rpgmvp/.png_
    ///
    /// .m4a => .rpgmvm/.m4a_
    Encrypt,

    /// Decrypts encrypted assets, deducing each file's key automatically
    ///
    /// .rpgmvo/.ogg_ => .ogg
    ///
    /// .rpgmvp/.png_ => .png
    ///
    /// .rpgmvm/.m4a_ => .m4a
    Decrypt,

    /// Extracts the key from the `--file` argument. Works on System.json or an encrypted RPG Maker file only
    ExtractKey,
}

/// Returns the `allowed` entry matching `path`'s extension, ignoring case. Canonical spelling, for matching against the extension constants.
fn matching_extension(path: &Path, allowed: &[&'static str]) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?;
    allowed
        .iter()
        .copied()
        .find(|candidate| candidate.eq_ignore_ascii_case(extension))
}

struct Processor<'a> {
    decrypter: Decrypter,
    command: AssetCommand,
    engine: AssetEngine,
    output_dir: &'a Path,
    input_dir: &'a Path,
    file: Option<&'a PathBuf>,
    global_key_set: bool,
    force: bool,
}

impl<'a> Processor<'a> {
    fn new(
        options: &'a AssetOptions,
        command: AssetCommand,
        input_dir: &'a Path,
        output_dir: Option<&'a Path>,
    ) -> Result<Self> {
        let mut decrypter = Decrypter::new();
        let mut engine = AssetEngine::MV;

        if let Some(file) = &options.file {
            if !file.is_file() {
                bail!("--file argument expects file as its argument.");
            }
        } else if command == AssetCommand::ExtractKey {
            bail!("--file argument is not specified.");
        }

        if let Some(key) = &options.key {
            decrypter.set_key_from_str(key)?;
        } else if command == AssetCommand::Encrypt {
            bail!("--key argument is not specified.");
        }

        if let Some(eng) = options.engine {
            engine = eng;
        } else if command == AssetCommand::Encrypt {
            bail!("--engine argument is not specified.");
        }

        let output_dir = match (output_dir, &options.file) {
            (Some(dir), _) => dir,
            // A bare file name has an empty parent, which is not a usable directory.
            (None, Some(file)) => file
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
            (None, None) => input_dir,
        };

        Ok(Self {
            decrypter,
            command,
            engine,
            output_dir,
            input_dir,
            file: options.file.as_ref(),
            global_key_set: options.key.is_some(),
            force: options.force,
        })
    }

    /// `extension` must be the canonical spelling returned by [`matching_extension`].
    /// Returns `Ok(false)` if the file was skipped because its output already exists.
    fn process_file(&mut self, file: &Path, extension: &str) -> Result<bool> {
        // `Path::extension` is `Some` only if `Path::file_name` is, and callers only pass paths with an extension.
        let file_name = unsafe { file.file_name().unwrap_unchecked() };

        if self.command == AssetCommand::Decrypt {
            // `extension` comes from `ENCRYPTED_ASSET_EXTS`, so it always has a file type.
            let file_type = FileType::try_from(extension).unwrap();
            let output = self
                .output_dir
                .join(file_name)
                .with_extension(file_type.decrypted_extension());
            if !self.force && output.exists() {
                return Ok(false);
            }

            let mut file_data = read(file)?;

            // Without a global key, each file may use its own, so derive it per file
            if !self.global_key_set {
                self.decrypter.set_key_from_file(&file_data, file_type)?;
            }

            let decrypted = self.decrypter.decrypt_in_place(&mut file_data, file_type)?;

            if file_type.check_plain(decrypted).is_err() {
                bail!(
                    "Decrypted {file_type} file has invalid signature. Check if you supplied correct key in `--key` argument."
                );
            }

            write(output, decrypted)?;
        } else {
            let file_type = match extension {
                PNG_EXT => FileType::Png,
                OGG_EXT => FileType::Ogg,
                M4A_EXT => FileType::M4a,
                _ => unreachable!(),
            };
            let new_extension = file_type.encrypted_extension(matches!(self.engine, AssetEngine::MZ));

            // Read directly behind the header, so the data is neither copied nor reallocated
            let mut input = File::open(file)?;
            let mut output_data =
                Vec::with_capacity(HEADER_LENGTH + usize::try_from(input.metadata()?.len()).unwrap_or(0));
            output_data.extend_from_slice(RPGM_HEADER);
            input.read_to_end(&mut output_data)?;

            self.decrypter
                .encrypt_in_place(&mut output_data[HEADER_LENGTH..], file_type)?;
            write(
                self.output_dir.join(file_name).with_extension(new_extension),
                output_data,
            )?;
        }

        Ok(true)
    }

    fn extract_key(&mut self) -> Result<()> {
        let Some(file_path) = self.file else {
            bail!("--file argument is not specified.");
        };

        if file_path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("System.json"))
        {
            let content = read_to_string(file_path)?;
            // Some editors prepend a BOM, which serde_json rejects.
            let system: Value = from_str(content.trim_start_matches('\u{feff}'))?;

            let Some(key) = system["encryptionKey"].as_str() else {
                bail!("`encryptionKey` not found in System.json. The game is probably not encrypted.");
            };

            println!("Encryption key: {key}");
        } else if let Some(extension) = matching_extension(file_path, ENCRYPTED_ASSET_EXTS) {
            let file_data = read(file_path)?;
            // `extension` is one of `ENCRYPTED_ASSET_EXTS`, which all have a file type.
            let key = self
                .decrypter
                .set_key_from_file(&file_data, FileType::try_from(extension).unwrap())?;
            println!("Encryption key: {key}");
        } else {
            bail!("Key can be extracted only from `System.json` file or RPG Maker encrypted file.");
        }

        Ok(())
    }

    fn process(&mut self) -> Result<()> {
        if self.command == AssetCommand::ExtractKey {
            return self.extract_key();
        }

        let allowed_extensions = if self.command == AssetCommand::Encrypt {
            DECRYPTED_ASSET_EXTS
        } else {
            ENCRYPTED_ASSET_EXTS
        };

        if let Some(file) = self.file {
            let Some(extension) = matching_extension(file, allowed_extensions) else {
                bail!("{} has unsupported extension.", file.display());
            };

            create_dir_all(self.output_dir)?;
            if self
                .process_file(file, extension)
                .with_context(|| file.display().to_string())?
            {
                println!("Processed 1 file.");
            } else {
                println!("Skipped {}: already exists. Use --force to overwrite.", file.display());
            }
        } else {
            // Read the input first, so a mistyped path fails without creating an empty output folder
            let entries = read_dir(self.input_dir)?;
            create_dir_all(self.output_dir)?;

            let mut processed: usize = 0;
            let mut skipped: usize = 0;
            let mut failed: usize = 0;

            for entry in entries {
                let path = match entry {
                    Ok(entry) => entry.path(),
                    Err(error) => {
                        eprintln!("{}: {error}", self.input_dir.display());
                        failed += 1;
                        continue;
                    }
                };
                let Some(extension) = matching_extension(&path, allowed_extensions) else {
                    continue;
                };

                // A single broken file shouldn't throw away the rest of the batch.
                match self.process_file(&path, extension) {
                    Ok(true) => processed += 1,
                    Ok(false) => skipped += 1,
                    Err(error) => {
                        eprintln!("{}: {error:#}", path.display());
                        failed += 1;
                    }
                }
            }

            println!("Processed {processed} file(s), skipped {skipped}.");

            if failed > 0 {
                bail!("{failed} file(s) failed to process.");
            }
        }

        Ok(())
    }
}

/// Runs an `asset` subcommand. `input_dir` and `output_dir` are the top-level `-i`/`-o` values.
pub fn execute(
    options: &AssetOptions,
    command: AssetCommand,
    input_dir: &Path,
    output_dir: Option<&Path>,
) -> Result<()> {
    Processor::new(options, command, input_dir, output_dir)?.process()
}
