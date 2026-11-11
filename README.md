# scanai

Put paper in the scanner and get a filed, searchable document. scanai watches a
network scanner, saves every scan, then turns the pages upright, OCRs them, and
has a local vision model work out what each document is.

Everything runs locally: the scanner is reached over your network, OCR runs on
your machine, and the vision model runs in [Ollama](https://ollama.com).

## What it does

**Watcher** (`src/watcher`)

- Scans automatically when paper is loaded into the document feeder (eSCL / AirScan).
- Appears on the printer panel under Scan → Computer (WSD), so pressing Start
  there scans to this machine, from the feeder or the glass.
- Saves each scan as JPEG pages plus a `.ron` metadata file (device, settings,
  page sizes, checksums).

**Post-processor** (`src/postprocess`)

- Turns each page upright and straightens crooked ones.
- Crops away empty glass around small items such as till slips and cards.
- Splits a scan of several pieces of paper laid on the glass into one document each.
- Stitches a long slip scanned in overlapping halves back into one image.
- OCRs every page (PP-OCRv6 through [oar-ocr](https://crates.io/crates/oar-ocr)).
- Asks the vision model for the document type, a summary, dates, parties,
  addresses, amounts, reference numbers, line items and tags.

The watcher starts post-processing after each scan, so running `scanai` does both.

## Requirements

- A scanner that speaks eSCL over HTTPS. Developed and tested with an Epson
  L5290 only; panel scanning also needs WSD (WS-Scan).
- Rust 1.95 or newer.
- [Ollama](https://ollama.com) with a vision model. The default is `qwen3-vl:8b`:

  ```sh
  ollama pull qwen3-vl:8b
  ```

The OCR models are downloaded automatically the first time post-processing runs.

## Setup

```sh
cargo install scanai
```

Or build from a checkout with `cargo build --release`; the binary is then
`target/release/scanai`.

Pick a directory for scanai to work in and put a `.env` file there with your
scanner's address (see [.env.example](.env.example)):

```sh
SCANAI_HOST=192.168.1.50
```

| Variable        | Meaning                                  | Default                         |
|-----------------|------------------------------------------|---------------------------------|
| `SCANAI_HOST`   | Scanner IP address or hostname           | none (required)                 |
| `SCANAI_OLLAMA` | Ollama server used for classification    | `http://localhost:11434`        |
| `SCANAI_DATA`   | Where scans and results are kept         | `data` in the current directory |

`.env` is read from the directory you run in, or one above it. Real environment
variables and command-line options take precedence.

Check that the scanner is reachable:

```sh
scanai watcher status
```

## Usage

```sh
scanai                          # watch the scanner and post-process every scan
scanai watcher once             # scan the feeder once and exit
scanai watcher once --source platen
scanai watcher status           # scanner details and current state

scanai postprocess run          # process everything not processed yet, then exit
scanai postprocess run ID       # process (or redo) one scan
scanai postprocess run --force  # redo every scan
scanai postprocess watch        # process new scans as they appear
scanai postprocess pieces ID    # show how a scan would be split, without processing
```

Scan settings (`--dpi`, `--color`, `--paper`, `--format`, `--intent`) go after
`watcher`, for example `scanai watcher --dpi 600 watch`. Run any command with
`--help` for the full list.

For panel scanning the printer connects back to this machine on port 5309
(`--wsd-port`), so that port must be open to it. Use `--no-wsd` to only watch
the feeder.

## Output

By default scans are written to `data/` in the directory you run in. Set
`SCANAI_DATA` to keep them somewhere else, or use `--out` and `--raw` for the
individual folders. A scan's id is the local time it started.

```
data/scanned_raw/<id>.ron         scan metadata
data/scanned_raw/<id>_p001.jpg    pages exactly as scanned

data/processed/<id>_p001.jpg      upright page
data/processed/<id>_p001.txt      OCR text
data/processed/<id>.ron           document type, summary and extracted fields
```

A scan is complete once its `.ron` file exists; it is always written last.
Pieces split from one scan are filed as `<id>_a`, `<id>_b`, and so on.

## Running as a service

A systemd user unit, saved as `~/.config/systemd/user/scanai.service`. Point
`WorkingDirectory` at the directory holding your `.env`:

```ini
[Unit]
Description=Scan and file documents loaded into the scanner

[Service]
WorkingDirectory=%h/scanai
ExecStart=%h/.cargo/bin/scanai watcher watch
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now scanai
journalctl --user -u scanai -f
```

To keep it running while logged out: `loginctl enable-linger $USER`.

## Notes

- The scanner's TLS certificate is not verified, because these devices use
  self-signed certificates. Use it on a network you trust.
- A scan whose post-processing fails (for example while Ollama is down) stays
  pending and is picked up again on the next run.

## Licence

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or
the [MIT licence](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 licence, shall
be dual licensed as above, without any additional terms or conditions.
