# rvpacker-txt-rs

[README на русском](./README-ru.md)

## General

This tool is designed to read RPG Maker game files into `.txt` files and write them back to their initial form.

This tool inherits its name from the original `rvpacker` tool, which was created for those versions of RPG Maker that did not use .json files, and parsed files into YAML. Now, `rvpacker`'s repository is deleted.

The same deprecated tool, written in Ruby, can be found in [rvpacker-txt repository](https://github.com/savannstm/rvpacker-txt).

There's [a GUI](https://github.com/rpg-maker-translation-tools/rpgmtranslate), that allows you comfortably edit files.

This CLI is written on top of [rvpacker-txt-rs-lib library](https://github.com/rpg-maker-translation-tools/rvpacker-txt-rs-lib).

We also have blazingly fast [asset](https://github.com/rpg-maker-translation-tools/rpgm-asset-decrypter-rs) and [archive decrypter](https://github.com/rpg-maker-translation-tools/rpgm-archive-decrypter) CLIs, you can use those to decrypt/encrypt assets and archives.

If you're not a fan of CLIs, you can use [a GUI](https://github.com/rpg-maker-translation-tools/rpgmdec) that combines asset and archives decrypters, it's very fast and light.

## The format of output files

`rvpacker-txt-rs` parses all the original text from the game's files, and inserts it on each new line of a text file. All line breaks (new lines, `\n`) are replaced by `\#` symbols.
At the end of each original line, `<#>` is inserted. This is a delimiter after which translated text should start. Removing it or erasing one of its symbols will lead to crashes, or worse, undefined behavior. **So remember: your translated text goes after the `<#>` delimiter.**

For an example on how to properly translate the .txt files, refer to [My Fear & Hunger 2: Termina Russian translation](https://github.com/savannstm/fh2-termina-translation).
Translation is Russian, but the point is to get how to properly translate this program's translation files.

## Installation

You can download executable for your system in Releases section.

## Usage

You can get help on usage by calling `rvpacker-txt-rs -h.`

Examples:

`rvpacker-txt-rs read -i "C:/Game"` parses the text of the game into the `translation` folder of the specified directory.

`rvpacker-txt-rs write -i "C:/Game"` writes the translation from `.txt` files of the `translation` folder to RPG Maker files in the `output` folder.

## License

Project is licensed under WTFPL.
