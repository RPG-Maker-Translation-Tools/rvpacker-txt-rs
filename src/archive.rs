use anyhow::{Context, Result, bail};
use clap::{
    Subcommand, ValueEnum,
    builder::{NonEmptyStringValueParser, PossibleValuesParser},
};
use encoding_rs::Encoding;
use rpgmad_lib::{Engine, EntrySource, decrypt_from_reader, encrypt_to_writer};
use std::{
    borrow::Cow,
    collections::HashSet,
    ffi::OsStr,
    fs::{File, create_dir_all, read_dir, remove_file, rename},
    io::{BufWriter, Read, copy, stdin},
    path::{Component, Path, PathBuf},
};
use walkdir::WalkDir;

const STANDARD_ENCRYPT_DIRS: &[&str] = &["audio", "data", "fonts", "graphics"];

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ArchiveEngine {
    Xp,
    Vx,
    Vxace,
}

#[derive(Debug, Subcommand)]
pub enum ArchiveCommand {
    /// Decrypt an .rgss archive. Input is the `-i` path to the archive or a directory containing it; output is `-o`
    Decrypt {
        /// Files to decrypt, delimited by colon, e.g. "Data/Actors.rvdata2:Graphics/Animations/Animation1.png". Both `/` and `\` work.
        #[arg(short, long, value_name = "FILENAMES", value_delimiter = ':')]
        decrypt_files: Vec<String>,

        /// Encoding of the file names inside the archive. Entries invalid in it are skipped with a warning; try `shift_jis` for non-ASCII names.
        #[arg(long, value_name = "ENCODING", default_value = "utf-8", value_parser = parse_archive_encoding)]
        encoding: &'static Encoding,
    },

    /// Encrypt RPG Maker Data/Graphics assets to an archive. Input is `-i`; output is `-o`, a file or a directory
    Encrypt {
        /// Engine to produce proper archive
        #[arg(short, long, value_name = "ENGINE")]
        engine: ArchiveEngine,

        /// Directories to encrypt, separated by comma.
        #[arg(short = 'E', long, value_name = "ENCRYPT_DIRS", value_parser = PossibleValuesParser::new(STANDARD_ENCRYPT_DIRS.iter().copied()), value_delimiter = ',', default_value = "data,graphics")]
        encrypt_dirs: Vec<String>,

        /// Additional directories to encrypt outside the standard `encrypt_dirs` ones, separated by comma.
        // An empty name would pack the whole input folder, so it's rejected.
        #[arg(
            long,
            value_name = "ADDITIONAL_ENCRYPT_DIRS",
            value_delimiter = ',',
            value_parser = NonEmptyStringValueParser::new()
        )]
        additional_encrypt_dirs: Option<Vec<String>>,

        /// Encoding for the file names in the archive, e.g. `shift_jis` for Japanese XP/VX games.
        #[arg(long, value_name = "ENCODING", default_value = "utf-8", value_parser = parse_archive_encoding)]
        encoding: &'static Encoding,
    },
}

impl ArchiveEngine {
    const fn archive_name(self) -> &'static str {
        match self {
            Self::Xp => "Game.rgssad",
            Self::Vx => "Game.rgss2a",
            Self::Vxace => "Game.rgss3a",
        }
    }

    const fn library_engine(self) -> Engine {
        match self {
            Self::Xp | Self::Vx => Engine::Older,
            Self::Vxace => Engine::Vxace,
        }
    }
}

/// Accepts any WHATWG encoding label that `encoding_rs` can also encode to (so not UTF-16).
fn parse_archive_encoding(label: &str) -> Result<&'static Encoding, String> {
    Encoding::for_label(label.as_bytes())
        .filter(|encoding| encoding.output_encoding() == *encoding)
        .ok_or_else(|| format!("unsupported encoding `{label}`"))
}

/// Whether `path` stays inside whatever directory it is joined to.
fn is_contained(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Whether `path` has an RPG Maker archive extension (`rgssad`, `rgss2a`, `rgss3a`). Case is ignored, as on Windows.
fn is_archive(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .and_then(|ext| ext.get(..4))
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("rgss"))
}

/// Picks the archive in `dir`: the first by name. Warns when there are several.
fn find_archive(dir: &Path) -> Result<PathBuf> {
    let mut archives: Vec<PathBuf> = read_dir(dir)
        .with_context(|| format!("{} is neither an .rgss archive nor a directory.", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_archive(path))
        .collect();

    // read_dir order is unspecified, sort so that the same directory always picks the same archive.
    archives.sort();

    if archives.len() > 1 {
        let names: Vec<_> = archives
            .iter()
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy())
            .collect();

        eprintln!(
            "Warning: found multiple archives ({}). To pick another one, pass it to --input-dir.",
            names.join(", ")
        );
    }

    archives
        .into_iter()
        .next()
        .context("No .rgss archive found in the directory.")
}

fn execute_decrypt(
    mut input_path: PathBuf,
    output_path: Option<PathBuf>,
    wanted_files: HashSet<String>,
    encoding: &'static Encoding,
) -> Result<()> {
    if !input_path.exists() {
        bail!("Input path does not exist.");
    }

    let output_path = output_path.unwrap_or_else(|| {
        if input_path.is_file() {
            // A bare file name has an empty parent, which is not a usable directory.
            input_path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        } else {
            input_path.clone()
        }
    });

    // A folder input: look for the archive inside it
    if !is_archive(&input_path) {
        input_path = find_archive(&input_path)?;
        println!("Using archive: {}", input_path.display());
    }

    // Created only once an archive is known, so a failed run leaves no empty folder behind.
    create_dir_all(&output_path)?;

    let mut archive = decrypt_from_reader(File::open(&input_path)?)?;

    // Names match without case, as on Windows, and `\` works as a separator.
    let mut wanted_files: HashSet<String> = wanted_files
        .into_iter()
        .map(|file| file.replace('\\', "/").to_ascii_lowercase())
        .collect();
    let selecting = !wanted_files.is_empty();
    let mut skipped: usize = 0;
    let mut last_dir: Option<PathBuf> = None;

    for index in 0..archive.entry_count() {
        // Never write a mangled name: it could collide with another entry and overwrite it.
        let Some(decoded) = encoding.decode_without_bom_handling_and_without_replacement(archive.path(index)) else {
            eprintln!(
                "Warning: skipped an entry, its name isn't valid {}: {}",
                encoding.name(),
                String::from_utf8_lossy(archive.path(index))
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

        if selecting && !wanted_files.remove(&path.to_ascii_lowercase()) {
            continue;
        }

        // The archive is untrusted: an absolute or `..` path would otherwise write outside `output_path`.
        if !is_contained(&path) {
            bail!("Archive entry {path:?} points outside the output directory.");
        }

        let output_file_path = output_path.join(&*path);

        // Entries come folder by folder, so creating each parent once is enough.
        if let Some(parent) = output_file_path.parent()
            && last_dir.as_deref() != Some(parent)
        {
            create_dir_all(parent)?;
            last_dir = Some(parent.to_path_buf());
        }

        // Content is copied straight from the archive, so only one buffer's worth is in memory at a time.
        let write_error = || format!("Failed to write {}", output_file_path.display());
        let mut output = File::create(&output_file_path).with_context(write_error)?;
        copy(&mut archive.read_entry(index)?, &mut output).with_context(write_error)?;
    }

    for missing in &wanted_files {
        eprintln!("{missing} was not found in the archive.");
    }

    if skipped > 0 {
        eprintln!(
            "Warning: {skipped} entries were skipped because their names aren't valid {}. Run again with a different \
             `--encoding`, e.g. `--encoding shift_jis`.",
            encoding.name()
        );
    }

    Ok(())
}

/// A file to store in the archive. Its content is read while the archive is written, not up front.
struct ArchiveFile {
    /// Path as stored in the archive, already encoded.
    archive_path: Vec<u8>,
    disk_path: PathBuf,
    size: usize,
}

impl EntrySource for ArchiveFile {
    fn path(&self) -> &[u8] {
        &self.archive_path
    }

    fn size(&self) -> usize {
        self.size
    }

    fn open(&self) -> std::io::Result<impl Read> {
        File::open(&self.disk_path)
    }
}

/// Streams `files` into a new archive at `path`, reading each file's content from disk as it goes.
fn write_archive(path: impl AsRef<Path>, files: &[ArchiveFile], engine: Engine) -> Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    encrypt_to_writer(files, engine, &mut writer)?;
    // Sync before the caller renames over the old archive, or a power cut can leave the new one empty.
    writer
        .into_inner()
        .map_err(std::io::IntoInnerError::into_error)?
        .sync_all()?;
    Ok(())
}

/// Lists the files of `encrypt_dirs` (under `input_path`) to store in the archive at `output_file`.
fn collect_files<'a>(
    input_path: &Path,
    output_file: &Path,
    encrypt_dirs: impl Iterator<Item = &'a String>,
    encoding: &'static Encoding,
) -> Result<Vec<ArchiveFile>> {
    let mut files: Vec<ArchiveFile> = Vec::with_capacity(128);
    let mut walked_dirs: HashSet<String> = HashSet::new();
    let mut added_paths: HashSet<Vec<u8>> = HashSet::new();

    for dir in encrypt_dirs {
        // Standard dir names are capitalized; others are used as typed.
        let mut dir_name = dir.clone();
        if STANDARD_ENCRYPT_DIRS.contains(&dir.as_str()) {
            dir_name[..1].make_ascii_uppercase();
        }

        // Listed twice (e.g. `-E data` and `--additional-encrypt-dirs data`), its files would go into the archive twice.
        if !walked_dirs.insert(dir_name.clone()) {
            continue;
        }

        let subdir = input_path.join(&dir_name);

        if !subdir.is_dir() {
            println!("{} is not a directory. It won't be encrypted.", subdir.display());
            continue;
        }

        // Follow symlinks, otherwise linked assets would be skipped without any warning.
        for entry in WalkDir::new(&subdir).follow_links(true).sort_by_file_name() {
            let entry = entry?;

            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();

            // Walking `.` reaches the old archive, which must not be packed into the new one.
            if path == output_file {
                continue;
            }

            // An absolute folder given to `--additional-encrypt-dirs` is not under `input_path`.
            let relative_path = path
                .strip_prefix(input_path)
                .with_context(|| format!("{} is outside the input directory.", path.display()))?;

            // RPG Maker archives use backslashes.
            let archive_path = relative_path
                .to_str()
                .with_context(|| format!("{} is not valid Unicode.", path.display()))?
                .replace('/', "\\");

            let (encoded_path, _, unmappable) = encoding.encode(&archive_path);

            if unmappable {
                bail!(
                    "{} can't be represented in {}. Pick another `--encoding`.",
                    path.display(),
                    encoding.name()
                );
            }

            // The same file can be reached through two listed folders, e.g. `graphics` and `Graphics/Pictures`.
            let stored_path = encoded_path.into_owned();
            if !added_paths.insert(stored_path.clone()) {
                continue;
            }

            // Only the size is needed now, the content is streamed into the archive later.
            let size = usize::try_from(
                entry
                    .metadata()
                    .with_context(|| format!("Failed to read {}", path.display()))?
                    .len(),
            )?;
            files.push(ArchiveFile {
                archive_path: stored_path,
                disk_path: path.to_path_buf(),
                size,
            });
        }
    }

    Ok(files)
}

fn execute_encrypt<'a>(
    input_path: &Path,
    output_path: Option<&Path>,
    engine: ArchiveEngine,
    encrypt_dirs: impl Iterator<Item = &'a String>,
    encoding: &'static Encoding,
) -> Result<()> {
    if !input_path.exists() {
        bail!("Input path does not exist.");
    }

    let archive_name = engine.archive_name();

    let output_file = match output_path {
        Some(path) if !path.is_dir() => path.to_path_buf(),
        Some(path) => path.join(archive_name),
        None => input_path.join(archive_name),
    };

    if output_file.exists() {
        let filename = output_file.file_name().unwrap();
        let mut input = String::with_capacity(4);

        println!(
            "{} already exists. Overwrite it? Input 'Y' to continue.",
            filename.display()
        );
        stdin().read_line(&mut input)?;

        if input.trim_end() != "Y" {
            bail!("{} was not overwritten.", filename.display());
        }
    }

    let files = collect_files(input_path, &output_file, encrypt_dirs, encoding)?;

    if files.is_empty() {
        bail!("No valid directories found (Data, Graphics). Nothing to encrypt.");
    }

    if let Some(parent) = output_file.parent() {
        create_dir_all(parent)?;
    }

    // Write beside the target and rename it over, so a failed write can't leave the old archive cut short.
    let mut temp_file = output_file.as_os_str().to_owned();
    temp_file.push(".tmp");

    if let Err(error) = write_archive(&temp_file, &files, engine.library_engine()) {
        // Don't leave a half-written archive behind.
        let _ = remove_file(&temp_file);
        return Err(error);
    }

    rename(&temp_file, output_file)?;
    Ok(())
}

/// Runs an `archive` subcommand. `input_dir` and `output_dir` are the top-level `-i`/`-o` values.
pub fn execute(command: &ArchiveCommand, input_dir: &Path, output_dir: Option<&Path>) -> Result<()> {
    match command {
        ArchiveCommand::Decrypt {
            decrypt_files,
            encoding,
        } => execute_decrypt(
            input_dir.to_path_buf(),
            output_dir.map(Path::to_path_buf),
            decrypt_files.iter().cloned().collect(),
            encoding,
        ),

        ArchiveCommand::Encrypt {
            engine,
            encrypt_dirs,
            additional_encrypt_dirs,
            encoding,
        } => execute_encrypt(
            input_dir,
            output_dir,
            *engine,
            encrypt_dirs.iter().chain(additional_encrypt_dirs.iter().flatten()),
            encoding,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::{SHIFT_JIS, UTF_8};
    use std::fs;

    #[test]
    fn rejects_paths_that_leave_the_output_directory() {
        assert!(is_contained("Data/Actors.rvdata2"));
        assert!(!is_contained(""));
        assert!(!is_contained("./Data"));
        assert!(!is_contained("../evil"));
        assert!(!is_contained("Data/../../evil"));
        assert!(!is_contained("/etc/passwd"));
    }

    #[test]
    fn encrypt_then_decrypt_restores_files() {
        let root = std::env::temp_dir().join(format!("rvpacker-archive-roundtrip-{}", std::process::id()));
        let input = root.join("game");
        let output = root.join("out");
        let contents = [
            ("Data/Actors.rvdata2", &b"actors"[..]),
            ("Graphics/Pictures/title.png", &b"png bytes"[..]),
        ];

        for engine in [ArchiveEngine::Xp, ArchiveEngine::Vx, ArchiveEngine::Vxace] {
            let _ = fs::remove_dir_all(&root);
            for (name, data) in contents {
                let path = input.join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, data).unwrap();
            }

            let dirs = ["data".to_string(), "graphics".to_string()];
            execute_encrypt(&input, None, engine, dirs.iter(), UTF_8).unwrap();
            execute_decrypt(
                input.join(engine.archive_name()),
                Some(output.clone()),
                HashSet::new(),
                UTF_8,
            )
            .unwrap();

            for (name, data) in contents {
                assert_eq!(fs::read(output.join(name)).unwrap(), data);
            }
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn shift_jis_names_roundtrip() {
        let root = std::env::temp_dir().join(format!("rvpacker-archive-sjis-{}", std::process::id()));
        let input = root.join("game");
        let output = root.join("out");
        let name = "Graphics/Pictures/タイトル.png";
        let path = input.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"png bytes").unwrap();

        let dirs = ["graphics".to_string()];
        execute_encrypt(&input, None, ArchiveEngine::Vx, dirs.iter(), SHIFT_JIS).unwrap();
        execute_decrypt(
            input.join(ArchiveEngine::Vx.archive_name()),
            Some(output.clone()),
            HashSet::new(),
            SHIFT_JIS,
        )
        .unwrap();

        assert_eq!(fs::read(output.join(name)).unwrap(), b"png bytes");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn selective_decrypt_ignores_case_and_separator() {
        let root = std::env::temp_dir().join(format!("rvpacker-archive-select-{}", std::process::id()));
        let input = root.join("game");
        let output = root.join("out");
        for (name, data) in [
            ("Data/Actors.rvdata2", &b"actors"[..]),
            ("Graphics/Pictures/title.png", &b"png bytes"[..]),
        ] {
            let path = input.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, data).unwrap();
        }

        let dirs = ["data".to_string(), "graphics".to_string()];
        execute_encrypt(&input, None, ArchiveEngine::Vxace, dirs.iter(), UTF_8).unwrap();
        execute_decrypt(
            input.join(ArchiveEngine::Vxace.archive_name()),
            Some(output.clone()),
            HashSet::from(["data\\ACTORS.rvdata2".to_string()]),
            UTF_8,
        )
        .unwrap();

        assert_eq!(fs::read(output.join("Data/Actors.rvdata2")).unwrap(), b"actors");
        assert!(!output.join("Graphics/Pictures/title.png").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn archive_is_found_whatever_the_extension_case() {
        let dir = std::env::temp_dir().join(format!("rvpacker-archive-find-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Game.RGSS3A"), b"").unwrap();

        assert_eq!(find_archive(&dir).unwrap(), dir.join("Game.RGSS3A"));
        let _ = fs::remove_dir_all(&dir);
    }
}
