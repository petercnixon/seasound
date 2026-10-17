# SeaSound

Cross-platform playlist music player. One window, one playlist, gapless playback. It is not a DAW and it does not manage a library.

License is MIT OR Apache-2.0. There is no README. Do not add one unless asked. Do not commit unless asked.

## Layout

Nothing below `seasound-ui` imports `egui`. `seasound-model` does not open files or talk to a device.

| Crate | Role |
| --- | --- |
| `crates/seasound-model` | Types, playlist, cursor, `PcmSource`. No I/O. |
| `crates/seasound-media` | Probe, tags, cue, chapters, gapless trim, Symphonia decode. |
| `crates/seasound-playback` | Ring, resampler, scheduler, cpal. An opener is injected. |
| `crates/seasound-ui` | eframe window. Sends `Command`s and draws a `Snapshot`. |
| `apps/seasound` | Binary name `seasound`. `main` calls `seasound_ui::run()`. |

Workspace: edition 2021, resolver 2, `rust-version` 1.86. This compiler has no let-chains (those landed in 1.88). `let`/`else` is fine. Do not lower `rust-version`. `.cargo/config.toml` sets `incompatible-rust-versions = "fallback"` so a newer crate release does not fail the resolve.

## Stack

- UI: eframe 0.31 / egui 0.31, rfd, `image` pinned to `=0.25.9`, `directories`.
- Decode: Symphonia 0.5 (`mp3`, `aac`, `isomp4`, `alac`, `aiff`, plus its defaults: flac, wav, ogg, vorbis). Tags via lofty. MP4 chapters and iTunSMPB via mp4ameta. Cue text via `encoding_rs` pinned to `=0.8.35`.
- Output: cpal 0.15, rubato when the file rate differs from the device, rtrb between the decode thread and the callback.

Do not bump eframe to 0.32. That pulls winit 0.31. File drop is fixed on winit 0.30 by opening through X11, not by upgrading.

Do not `cargo fmt` the whole tree. Do not `cargo update` unless a compile failure forces a pin, and then pin the smallest crate that 1.86 can build.

## What plays

MP3, AAC-LC in M4A/M4B/MP4, FLAC, ALAC, WAV, AIFF, and Vorbis in Ogg. HE-AAC (Audio Object Type 5 or 29) is an error row. Opus is an error row. The probe also accepts a sibling `.cue`, a `.cue` dropped on its own, and `.m3u` / `.m3u8`.

Decoder plugins are shared libraries (`libseasound_*.so`). The host is `seasound-media`; the ABI is `seasound-plugin-api`. Built-in extensions are never handed to a plugin. Plugins are scanned from `SEASOUND_PLUGIN_DIR`, then `plugins/` next to the executable, then `plugins/` in the config directory. The first plugin id wins. A plugin is used when it loaded and its id is not in `settings.json` `disabled_plugins`. The Plugins menu toggles that. The WavPack plugin (`plugins/seasound-decoder-wavpack`, id `wavpack`) plays lossless mono and stereo `.wv`. It decodes the file into memory on open. `third_party/wavicle` is wavicle 0.1.0 with `is_multiple_of` rewritten so Rust 1.86 can build it. Do not raise `rust-version` for that.

Each playlist row is one `Segment` on a file's presentation timeline. `start_frame` is inclusive and `end_frame` is exclusive, after encoder delay has been removed. A normal file is one row. A cue or a chapter list is one row per track.

`SourceId::fresh()` is a process-wide counter starting at 1. Ids are never reused. Do not restart them at 1, and do not assert an exact id. A reused id keeps the previous decoder open, which is why opening a new file used to keep playing the old one.

## Gapless playback

One cpal stream for the life of the device. Pause writes silence. It does not close the device. The callback only reads the ring, applies a squared volume ramp of about 10 ms, and never allocates. A device change starts a new stream with the ramp at silence.

Symphonia's `enable_gapless` stays off in both the probe and the decoder. `PcmSource::pull` returns presentation frames. Delay and padding are already gone. Cue points and chapter points use that same clock.

MP3 trim comes from the LAME or Lavf tag in the first audio frame (12-bit delay and padding at marker + 21). `parse_lame_delay` returns those raw tag values. A layout test expects delay 576 and padding 1560 for the example album. Do not change that function to return the shifted values.

`finish_mp3` adds the ISO decoder delay when it uses the LAME tag:

- `MP3_DECODER_DELAY` is 529 (528 filterbank samples plus the extra sample LAME and ffmpeg count).
- Playback delay is tag delay + 529. Padding is tag padding − 529. Audible length is unchanged when padding is at least 529.
- iTunSMPB is not shifted. It is kept only when the LAME delay is 0 and its three counts match the Xing frame count within 1152.
- AAC priming from iTunSMPB or the edit list is already an absolute count. Do not add 529, and do not guess 2112 for an MP3.
- Chapter starts are not shifted by encoder delay.

On the example album every probed file has a raw delay of 576. ffmpeg's `start_time` of 0.025057 s is 1105 samples. Track 101's Xing count is 8816256 frames. The presentation length is 8814120. A skip of 576 is about 529 samples early and does not match ffmpeg.

An MP3 timestamp seek, whether Symphonia's or ffmpeg `-ss`, is not sample-accurate on the tail. A measured miss was about 0.077. Prove a tail by decoding the file straight through. The example's intro is digital silence, so a 0.05 s compare matches at every skip. Compare at least 0.5 s.

Same-file segments that abut do not seek and do not reset the decoder. A different file at the same rate keeps the resampler. A rate change, or repeat-one landing on the same index, does not continue.

`PlayerCore::prepare` returns immediately when the active decoder is already at or past `job.end` and `force_seek` is clear. That guard is load-bearing. Removing it re-seeks a finished segment.

The LAME `--nogap` bit reservoir is not passed across files. Leave it alone unless a pop remains after the 529-sample shift and the user asks for that work.

## Playlist behavior

- Add files and Add folder append. Open files and Open folder replace. The empty-state button is Open files. A file drop appends.
- Clear playlist empties the window and drops the open file name. It does not write that file. New playlist writes the current place into the open file first, then does the same, so the next Save starts from `playlist.json`.
- A playlist file stores each row's path, title, artist, album, timing, and the decode trim (`delay`, `padding`, `presentation_frames`). When every row has a sample rate and an end frame, Open and session restore build the list from that JSON and do not read the media files. Cover art is not in the file, so those loads show no art. The decoder opens a file when it plays. A file that fails to open stays in the list with the error on the row, and playback moves to the next playable row.
- Older playlist files that only stored path, start frame, and title still probe. The saved file is probed first, then the files after it, then the files before it. Each finished file is added immediately, and `ReplacePlaylist` keeps the playing source id. A short track can still end before the next file has been probed.
- While files are still being read, the playlist shows `Loading…` (with a count when one is known) in the status line and above the rows. The overlay does not cover the list, and row clicks still work.
- Open files, Open folder, Add, and a drop probe one path at a time. Each source is appended as it finishes. The first of those starts playback when the list was empty. A folder is listed, cue sheets are expanded first, and the remaining audio stays in path order. `load_paths` still sorts a folder by album, disc, and track, and is not the path the window uses.
- Shuffle changes play order only. It does not reorder the visible rows. Repeat is off, one, or all.
- Dragging a row that is not selected selects a contiguous range. Dragging a row that is already selected moves the whole selection. Travel under 8 px is a click, and a click in move mode keeps only that row. Double-click plays that row.
- Past the threshold, a move draws an accent insertion line, an accent stroke on the picked-up rows, and a foreground label (the title, or `N tracks`).
- Delete sends `RemoveMany` for every selected index. Removing the current row starts the next remaining playable row. A single `Remove` that empties the playlist does not halt or release the decoder. Leave that unless asked.
- `MoveRows` keeps playing the same file. It does not restart output. `Reorder` does not update `display_index`. `MoveRows` and `RemoveMany` do.
- The playlist `ScrollArea` uses `id_salt("playlist")` and `drag_to_scroll(false)`. Row hit testing uses this frame's rectangles because `show_rows` virtualizes. Drive the gesture from the pointer, not from `response.dragged()`.

## File drop

winit 0.30 emits no drop events on Wayland. `seasound-ui` forces the X11 backend when `WAYLAND_DISPLAY` and `DISPLAY` are both non-empty, unless `WINIT_UNIX_BACKEND` is `wayland`. The unix-only winit dependency requests the `x11` feature and matches eframe's resolved 0.30.13. Do not write a Wayland data-device client. A pure Wayland session with an empty `DISPLAY` still will not see drops.

## Settings

`directories::ProjectDirs::from("audio", "SeaSound", "SeaSound")`. The Linux path is lowercased, so it is `~/.config/seasound`. `settings.json` stores volume, mute, the output device, `disabled_plugins`, and `resume_playback` (default on). When resume is on, `session` stores the in-app playlist rows, the current track (`path` and `start_frame`), and `position_frames`. The next launch loads those rows and plays that track from that position. It does not reload a playlist file the user opened. The saved playlist path stays attached, so exit still updates that file. An empty `session` means the playlist was cleared. With no `session` yet, startup still loads `playlist` from settings and does not autoplay. `playlist.json` is the session list, written on autosave only after the window has shown rows. A playlist file stores those rows plus an optional `position` (`path`, `start_frame`, `position_frames`). Rows also store artist, album, end frame, sample rate, channels, bitrate, bit depth, delay, padding, presentation length, track number, format name, and an optional group. Empty values are omitted so an older file still parses. Save playlist writes the position when a track is current. Open playlist plays that track from there without reading the media when the rows are complete. A file with no `position` loads and does not start. The startup fallback that loads `playlist` from settings does not use `position`. Open files, Open folder, Open playlist, and New playlist write the attached file first, then let go of it, so the next playlist does not overwrite that place. Clear lets go of the file without writing it. Add files and Add folder stay on the same file. The window size is eframe's own persistence. The Settings menu opens the settings modal. eframe's `persistence` feature is off, so `App::save` does not run. The window writes `settings.json` when a close is requested. A test launch must set `XDG_CONFIG_HOME` to a temp directory so it does not write the user's config.

## Examples and tests

`examples/` is gitignored and is copyrighted. Do not commit it, and do not copy it into the repo. Tests skip when it is absent. From `crates/seasound-media`, the path is `CARGO_MANIFEST_DIR/../../examples`.

The three fixtures are `examples/Gapless MP3 Album/` (22 LAME 3.93 MP3s, `101` through `122`), `examples/FLAC with embedded CUE.flac`, and `examples/ACC with chapters.m4a`. The M4A has a Nero `chpl` chapter list and no QuickTime chapter track. Paths contain spaces and apostrophes. Pass them as process arguments, not through a shell.

FLAC cue: a binary cuesheet block (type 5) supplies sample offsets. Vorbis `CUESHEET` text supplies `TITLE` and `PERFORMER`. A sibling `.cue` with the same stem is used when the file has no embedded text. A bad sheet becomes one whole-file row plus a warning. Index times are `MM:SS:FF` at 75 fps. Bytes are UTF-8, then Windows-1252.

M4A chapters: the QuickTime chapter track wins when it is non-empty. Otherwise the Nero `chpl` atom is used. Chapter times are durations, so a highlight can be a frame or two off. Playback of that file is still one continuous decode.

```
cargo test -p seasound-model -p seasound-playback -p seasound-ui --offline --lib
cargo test -p seasound-media --offline --lib
```

The media suite decodes the example MP3 and takes several seconds. `MemoryOutput::new` and `samples_handle` warn as dead code on a lib build. An unused `MemoryOutput` import in `engine.rs` warns too. The tests use them. Do not `cargo fix` those warnings.

Prefetch opens the next different file at the start of a segment. An engine test of two files often opens the upcoming file before it plays. Assert the open count after it has settled. Do not require one extra open on a command that did not start a new file.

This is a native window, not a web app. Verify UI changes by running `target/debug/seasound` and, when a pointer action matters, driving it. `import -window` can capture the X11 client. Kill only the pid from `pgrep -x seasound`. `pkill -f` matches the shell that launched it.

## Out of scope

Library view, tag editing, EQ, ReplayGain, loudness normalization, lyrics, streaming, CD ripping, HE-AAC, Opus, a web build, the LAME bit reservoir, output or DSP plugins, and unloading a plugin while it is in use.
