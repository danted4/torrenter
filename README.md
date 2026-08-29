# WebTorrent CLI Client

**Node.js terminal downloader for WebTorrent magnet links and multipart HTTP(S) direct URLs** — live progress bar, peer stats, speed, ETA, and resume.

> **Keywords:** WebTorrent, torrent, magnet link, magnet URI, BitTorrent, P2P, Node.js CLI, terminal downloader, HTTP range requests, multipart download, resume download, progress bar, peer tracking.

Formerly **[nodeWT](https://github.com/danted4/nodeWT)** — renamed for searchability.

[![Node.js](https://img.shields.io/badge/node-%3E%3D22.0.0-brightgreen)](https://nodejs.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![WebTorrent](https://img.shields.io/badge/WebTorrent-2.1.27-orange)](https://webtorrent.io/)

## Table of contents

- [Why this exists](#why-this-exists)
- [Features](#features)
- [Quick start](#quick-start)
- [Usage](#usage)
  - [Option 1 — Magnet URL (WebTorrent)](#option-1--magnet-url-webtorrent)
  - [Option 2 — Direct URL (multipart)](#option-2--direct-url-multipart)
- [Progress bar](#progress-bar)
- [How it works](#how-it-works)
- [Configuration](#configuration)
- [Project layout](#project-layout)
- [Troubleshooting](#troubleshooting)
- [Responsible use](#responsible-use)
- [Contributing](#contributing)
- [License](#license)

---

## Why this exists

Most GUI torrent clients are heavy. This project is a **small Node.js CLI** that wraps [WebTorrent](https://webtorrent.io/) for magnet-link downloads with a clean terminal UI — progress bar, peers, speed, and ETA.

Since v2.0 it also supports **direct HTTP(S) downloads** using parallel byte-range parts (with resume) when the server allows it — useful for large files without a torrent.

---

## Features

| | Magnet (WebTorrent) | Direct URL |
|---|---------------------|------------|
| **Input** | `magnet:?xt=urn:btih:…` | `https://…` |
| **Protocol** | BitTorrent / WebTorrent P2P | HTTP(S) with optional ranges |
| **Parallelism** | WebTorrent piece picker | 4 concurrent range parts |
| **Resume** | WebTorrent built-in | Part files in `.downloads/.nodewt/` |
| **Progress** | Peers, speed, ETA | Speed, ETA (peers shown as `0`) |

- Real-time **cli-progress** bar with formatted size and speed
- **Magnet URL** and **HTTP(S) URL** validation
- Spinner during torrent client initialization
- **Node.js 22+** (uses native `fetch` for direct downloads)
- Files saved to `./downloads/`

---

## Quick start

### Prerequisites

- **Node.js 22+** ([nodejs.org](https://nodejs.org/) or `nvm install 22`)
- **npm** or **yarn**
- macOS, Linux, or Windows

### Install

```bash
git clone https://github.com/danted4/webtorrent-cli-client.git
cd webtorrent-cli-client
npm install
```

### Run

```bash
npm start
```

You will be prompted to choose a download type (magnet or direct URL).

---

## Usage

### Option 1 — Magnet URL (WebTorrent)

```bash
npm start
```

```
Choose download type:
1. Magnet URL
2. Direct URL (multi-part)

1

Please paste your magnetURI:

magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10
```

The client connects to peers via WebTorrent, downloads to `./downloads/`, and exits when complete.

### Option 2 — Direct URL (multipart)

```bash
npm start
```

```
Choose download type:
1. Magnet URL
2. Direct URL (multi-part)

2

Please paste the direct download URL:

https://example.com/large-file.iso
```

**When the server returns `Content-Length` and `Accept-Ranges: bytes`:**

- Downloads **4 parts in parallel**
- **Resumes** interrupted parts from `.downloads/.nodewt/`
- Merges parts into the final file in `./downloads/`

**When ranges are not supported:** falls back to a single streamed download.

Filename is taken from `Content-Disposition` or the URL path.

---

## Progress bar

```
[████████████████░░░░] 75% | Peers: 12 | D: 750.45 MB @ 2.5 MB/s | T: 1.0 GB | ETA: 2 minutes
```

| Field | Meaning |
|-------|---------|
| Bar / `%` | Download progress |
| `Peers` | Connected peers (magnet mode only) |
| `D` | Downloaded so far |
| `@ speed` | Current throughput |
| `T` | Total size |
| `ETA` | Estimated time remaining |

---

## How it works

```mermaid
flowchart TD
  A[npm start] --> B{Download type?}
  B -->|1 Magnet| C[Validate magnet URI]
  C --> D[WebTorrent client]
  D --> E[Peer discovery + download]
  E --> F[./downloads/]
  B -->|2 Direct URL| G[Validate HTTP(S) URL]
  G --> H{Server supports ranges?}
  H -->|Yes| I[4 parallel range parts + resume]
  H -->|No| J[Single stream]
  I --> K[Merge parts]
  J --> F
  K --> F
```

**Magnet path:** `server.js` → `startDownload()` → WebTorrent → progress events → `./downloads/`

**Direct path:** `server.js` → `startURLDownload()` → `fetch` with `Range` headers → merge → `./downloads/`

---

## Configuration

### Download folder

Default: `./downloads`. Change in `src/client/index.js`:

```javascript
const DOWNLOAD_DIRECTORY = './downloads';
```

For magnet downloads, also update the `path` in `client.add(…)`.

### Progress bar format

Edit `getDisplayFormat()` in `src/utils/formatters/index.js`.

### Node version

Enforced in `package.json` (`>=22.0.0`). Use `.nvmrc` with nvm:

```bash
nvm use
```

---

## Project layout

```
webtorrent-cli-client/
├── server.js              # CLI prompts (magnet or direct URL)
├── src/
│   ├── client/index.js    # startDownload(), startURLDownload()
│   └── utils/               # formatters, loaders, logger, validators
├── downloads/             # Output directory (gitignored contents)
├── package.json
├── CHANGELOG.md
└── LICENSE
```

---

## Troubleshooting

| Problem | Fix |
|---------|-----|
| `Invalid magnet URL` | Link must start with `magnet:?xt=urn:` and include a valid info hash |
| No peers / slow torrent | Wait for DHT discovery; try a well-seeded magnet; check firewall |
| `Invalid HTTP(S) URL` | Use a full `http://` or `https://` URL |
| Direct download fails on ranges | Server may block ranges — tool falls back to single stream automatically |
| `node: command not found` / version error | Install Node 22+: `node --version` should show `v22.x` |
| Resume not working | Do not change the URL between runs; resume state lives in `.downloads/.nodewt/` |

### Debug mode (show Node warnings)

```bash
# macOS / Linux
NODE_NO_WARNINGS=0 node server.js

# Windows
set NODE_NO_WARNINGS=0 && node server.js
```

---

## Responsible use

- Download only **content you have the legal right to access**
- This tool is a **generic downloader** — not affiliated with WebTorrent beyond using their open-source library
- Authors are not responsible for misuse

---

## Contributing

Issues and PRs welcome. Please do not open issues with copyrighted material or magnet links to illegal content.

1. Fork the repo
2. Create a branch: `git checkout -b feature/your-feature`
3. Commit and push
4. Open a Pull Request

---

## License

[MIT](LICENSE)

---

**Made by [danted4](https://github.com/danted4)** · Formerly **nodeWT**
