# Read-only file previews

`file_viewer` registers before text-buffer loading and recognizes the 191 extensions
in `src/formats.rs`, matching the P4J format inventory. Every recognized local file
has a read-only tab and a paged Hex view. Unsupported content and parser failures
automatically open Hex with the original reason visible. Unknown local binary files
also open Hex after a bounded file-header check. Text and code continue to use the editor.

| Family | Content available |
| --- | --- |
| SQLite | Schema and DDL, tables/views, columns, indexes, triggers, typed records and incremental TEXT/BLOB prefixes |
| Excel / ODS / ET | Worksheets and cells, shared strings, formulas/cached results; native OOXML, XLSB, BIFF8 and ODS readers |
| Word | Paragraphs, table text, headers/footers, notes and comments from OOXML parts |
| PowerPoint / DPS | Slides and speaker notes from OOXML and legacy PowerPoint atoms |
| Pictures | Bounded still-image previews, PSD/PSB composites and independent layer lists; less common codecs use FFmpeg or ImageMagick |
| GPU textures | Container and mip content from KTX/KTX2/PVR; supported encodings decode one bounded mip at a time |
| Audio / video | Seekable waveform/frame pages, stream metadata and streaming audio playback; video auto frames advance at one frame per second |
| PDF | One rasterized page at a time and document metadata, using `pdfinfo` / `pdftoppm` |
| Models | OBJ/STL/PLY/OFF paged wireframes and geometry; PLY/OFF vertex and face tables; glTF/GLB scene, meshes, materials, accessors and animations; textual formats have byte-accurate content pages |
| Executables / libraries / PDB | Architectures, sections, imports/exports, symbols, type/module/source records and embedded signature metadata |
| Unreal | Versioned names, imports, exports, graph/function/property object types and dependencies |
| MAX | Compound streams with paged actual stream bytes |

Unknown encodings within a recognized family automatically open Hex. Proprietary
WPS variants, unsupported model encodings and unsupported texture compression do
not claim full application rendering. Large legacy compound files that exceed the
index budget use byte pages. Word/PowerPoint layouts and embedded pictures are not
reproduced. MAX/Unreal payloads are inspectable without running scene scripts or
Blueprint code. Remote binary transport is not implemented; remote tabs explain
that a local file is needed.

FFmpeg/FFprobe must be on PATH for media conversion/playback. ImageMagick supplies
additional picture codecs when available; external delegates are disabled, and
codec availability depends on the installed build. PDF tools must be on PATH for
PDF page rendering. Neither tools nor conversion results modify the source.

## Resource budgets

Files strictly larger than 500 MB (500,000,000 bytes), including text files, open
in a read-only Hex preview before any complete loading or format parsing. Each
page reads at most 3200 bytes. “Open all content” displays a confirmation with the
file size and memory risk; cancelling retains the preview. Confirmed text opens
in the editor, while confirmed structured formats keep their paging and decoder
budgets. Confirmation is scoped to the current opening and file size: growth and
restoring or splitting a preview require confirmation again. Direct buffer loads
also enforce the threshold. Remote full-content confirmation is unavailable,
and the existing 6 GiB text loading limit remains in place.

Only one synchronous parser runs across the app at a time. Pages hold at most 200
rows / 2 MiB of text with 4 KiB cell prefixes, and the view virtualizes rows, section
lists and metadata. Image output is PNG with each edge at most 1024 pixels. Native
image input is capped at 8 MiB per decoding pass, and pixel decoding requests a
64 MiB allocation limit. PSD sampling, ZIP/CFB indexes, SQLite schema reads and
binary random-read caches have separate fixed bounds before allocation.
SVG parsing limits reference expansion, gradient/CSS copies and render surfaces;
font text, embedded raster images and external resources are omitted. SVG and
compressed texture decoders run in isolated workers before GUI/crash initialization.
Unsupported binary encodings and parser errors automatically open Hex. Hex pages read at
most 3200 source bytes and show offsets, hexadecimal values and ASCII; navigation
and refresh preserve Hex mode.

SQLite uses an immutable, read-only connection with a 2 MiB page cache, disabled
memory mapping, query-only mode, authorizer restrictions and time/instruction
budgets. It does not merge uncheckpointed WAL/journal changes, execute virtual
tables or create source sidecars. Source and sidecar stamps are checked around
queries. A changing database must be refreshed after writes finish.

External decoders have bounded stdout/stderr, a 10-second preview deadline and a
64 MiB FFmpeg single-allocation limit. Linux/FreeBSD also apply an address-space
limit. macOS rejects AS/DATA/RSS rlimits, so actual worker resident memory/footprint
is monitored every 10 ms against 256 MiB (20 ms during playback); this is a watchdog
and can have sampling overshoot. Windows attaches workers to Job Objects with
256 MiB committed-memory limits and tree termination, and monitors peak working
set. Association occurs just after spawn. Playback has a fixed PCM
queue and one audio decoder across the process. Closing/navigating stops playback;
obsolete page results are discarded. Decoder process groups are reaped on timeout.
Waveform pages limit input to ten seconds before filtering and normalize it to
stereo at 44100 Hz. Audio source sample rates and channel counts have fixed limits.
The Windows containment test requires a Windows host; validation on this macOS
host does not cover that platform.

The library contains sparse-file, malformed-container, oversized-cell, read-only
sidecar, pagination and actual-format regression tests. For independent read-only
memory measurements, build and run `preview_probe` with an existing fixture:

```sh
cargo build -p file_preview --example preview_probe
/usr/bin/time -l target/debug/examples/preview_probe /path/to/file.sqlite table:example
/usr/bin/time -l target/debug/examples/preview_probe /path/to/large.max Bytes
```

Create large fixtures in a separate process so fixture construction is excluded
from the measured preview peak.

On this macOS host, an independent reader of a database containing a 256 MiB BLOB
peaked at about 7.4 MiB RSS; a 64 GiB sparse-file byte page peaked at about 4.6 MiB.
These measurements exclude the Zed UI and fixture construction. GPUI regression
tests draw the view and exercise clicks, copying, image caches and window cleanup;
audio-device output and a complete native Zed window have not been validated here.
