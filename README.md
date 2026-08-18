# taffle

`taffle` converts ordinary audio files (MP3, M4B, WAV, …) into Tonie Audio Format (TAF)
files that a Toniebox can play: it decodes the input, resamples it to 48 kHz, encodes
Opus, assembles the TAF container with its chapter table, and writes the result. The
workspace also ships the reusable pieces separately — a `no_std` core for reading and
validating TAF, an encoder for writing it, the conversion library, and the `taffle`
command-line tool.

**Under construction.** Nothing here works yet; the crates are scaffolding.

## License

Licensed under the MIT license ([LICENSE](LICENSE)).
