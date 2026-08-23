# 🌊 nodeWT - WebTorrent CLI Client

```
 ███▄    █  ▒█████  ▓█████▄ ▓█████  █     █░▄▄▄█████▓
 ██ ▀█   █ ▒██▒  ██▒▒██▀ ██▌▓█   ▀ ▓█░ █ ░█░▓  ██▒ ▓▒
▓██  ▀█ ██▒▒██░  ██▒░██   █▌▒███   ▒█░ █ ░█ ▒ ▓██░ ▒░
▓██▒  ▐▌██▒▒██   ██░░▓█▄   ▌▒▓█  ▄ ░█░ █ ░█ ░ ▓██▓ ░
▒██░   ▓██░░ ████▓▒░░▒████▓ ░▒████▒░░██▒██▓   ▒██▒ ░
░ ▒░   ▒ ▒ ░ ▒░▒░▒░  ▒▒▓  ▒ ░░ ▒░ ░░ ▓░▒ ▒    ▒ ░░
░ ░░   ░ ▒░  ░ ▒ ▒░  ░ ▒  ▒  ░ ░  ░  ▒ ░ ░      ░
   ░   ░ ░ ░ ░ ░ ▒   ░ ░  ░    ░     ░   ░    ░
         ░     ░ ░     ░       ░  ░    ░
```

A powerful, feature-rich Node.js CLI wrapper for WebTorrent with real-time progress tracking, peer statistics, and beautiful terminal UI.

[![Node.js Version](https://img.shields.io/badge/node-%3E%3D22.0.0-brightgreen)](https://nodejs.org/)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![WebTorrent](https://img.shields.io/badge/WebTorrent-2.1.27-orange)](https://webtorrent.io/)

---

## 📋 Table of Contents

- [Features](#-features)
- [Architecture](#-architecture)
- [Project Structure](#-project-structure)
- [Installation](#-installation)
- [Usage](#-usage)
- [How It Works](#-how-it-works)
- [Configuration](#-configuration)
- [Development](#-development)
- [Troubleshooting](#-troubleshooting)
- [Contributing](#-contributing)
- [License](#-license)

---

## ✨ Features

- 🚀 **Fast Downloads**: Leverages WebTorrent protocol for efficient P2P file sharing
- 📊 **Real-time Progress**: Beautiful CLI progress bar with live statistics
- 👥 **Peer Tracking**: Monitor connected peers in real-time
- 📈 **Speed Metrics**: Track download speed with auto-formatted units (B/s, KB/s, MB/s, GB/s)
- ⏱️ **ETA Calculation**: Smart time estimation with human-readable format
- 🎨 **Animated Spinner**: Visual feedback during torrent initialization
- ✅ **Magnet URL Validation**: Built-in validation for magnet links
- 📦 **Direct URL Downloads**: Download HTTP(S) files with four parallel byte-range parts when supported
- 🔄 **Auto-formatting**: Intelligent size and speed formatting
- 🌐 **Node.js 22 LTS**: Built for the latest Node.js LTS version

---

## 🏗️ Architecture

### High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         USER INTERFACE                          │
│                      (Terminal/CLI Input)                       │
└────────────────────────────┬────────────────────────────────────┘
                             │
                             ▼
┌─────────────────────────────────────────────────────────────────┐
│                        server.js (Entry)                        │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │  • Readline Interface for User Input                     │   │
│  │  • Magnet URL Validation                                 │   │
│  │  • Initiates Download Process                            │   │
│  └──────────────────────────────────────────────────────────┘   │
└────────────────────────────┬────────────────────────────────────┘
                             │
                             ▼
┌─────────────────────────────────────────────────────────────────┐
│                    CLIENT MODULE (src/client)                   │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │  WebTorrent Client Manager                               │   │
│  │  • Initialize WebTorrent instance                        │   │
│  │  • Add torrent from magnet URI                           │   │
│  │  • Monitor download events                               │   │
│  │  • Update progress bar                                   │   │
│  │  • Handle completion/errors                              │   │
│  └──────────────────────────────────────────────────────────┘   │
└────────────────────────────┬────────────────────────────────────┘
                             │
                             ▼
┌─────────────────────────────────────────────────────────────────┐
│                   UTILITIES MODULE (src/utils)                  │
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐           │
│  │  FORMATTERS  │  │   LOADERS    │  │   LOGGER     │           │
│  ├──────────────┤  ├──────────────┤  ├──────────────┤           │
│  │ • formatETA  │  │ • Spinner    │  │ • log()      │           │
│  │ • formatSize │  │   Animation  │  │ • error()    │           │
│  │ • formatSpeed│  │ • Progress   │  │ • info()     │           │
│  │ • getDisplay │  │   Indicator  │  │              │           │
│  │   Format     │  │              │  │              │           │
│  └──────────────┘  └──────────────┘  └──────────────┘           │
│                                                                 │
│  ┌──────────────┐                                               │
│  │  VALIDATORS  │                                               │
│  ├──────────────┤                                               │
│  │ • isValid    │                                               │
│  │   MagnetURL  │                                               │ 
│  └──────────────┘                                               │
└─────────────────────────────────────────────────────────────────┘
                             │
                             ▼
┌─────────────────────────────────────────────────────────────────┐
│                      WEBTORRENT LIBRARY                         │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │  • DHT (Distributed Hash Table)                          │   │
│  │  • Peer Discovery & Connection                           │   │
│  │  • Piece Management                                      │   │
│  │  • File System Operations                                │   │
│  └──────────────────────────────────────────────────────────┘   │
└────────────────────────────┬────────────────────────────────────┘
                             │
                             ▼
┌─────────────────────────────────────────────────────────────────┐
│                      FILE SYSTEM (./downloads)                  │
│                     Downloaded Torrent Files                    │
└─────────────────────────────────────────────────────────────────┘
```

### Data Flow Diagram

```
┌──────────┐
│  User    │
│  Input   │
└────┬─────┘
     │ Magnet URI
     ▼
┌─────────────────┐
│   Validator     │──────► Validate Format
└────┬────────────┘
     │ Valid URI
     ▼
┌─────────────────┐
│   Spinner       │──────► "Initializing..."
│   Animation     │
└─────────────────┘
     │
     ▼
┌─────────────────┐
│  WebTorrent     │
│  Client Init    │
└────┬────────────┘
     │
     ▼
┌─────────────────┐
│  Add Torrent    │──────► Connect to Peers
└────┬────────────┘
     │
     ▼
┌─────────────────────────────────────────┐
│         Download Event Loop             │
│  ┌───────────────────────────────────┐  │
│  │  1. Get torrent stats             │  │
│  │  2. Format data (size, speed, ETA)│  │
│  │  3. Update progress bar           │  │
│  │  4. Check completion              │  │
│  └───────────────────────────────────┘  │
└────┬────────────────────────────────────┘
     │
     ▼
┌─────────────────┐
│  Download       │
│  Complete       │──────► Exit Process
└─────────────────┘
```

---

## 📁 Project Structure

```
nodeWT/
│
├── 📄 server.js                    # Entry point - CLI interface
│   └── Handles user input and initiates download
│
├── 📁 src/
│   ├── 📁 client/
│   │   └── 📄 index.js             # WebTorrent client manager
│   │       ├── startDownload()     # Main download orchestrator
│   │       ├── Progress tracking   # Real-time statistics
│   │       └── Event handlers      # Download/error events
│   │
│   └── 📁 utils/
│       ├── 📄 index.js             # Utility exports aggregator
│       │
│       ├── 📁 formatters/
│       │   └── 📄 index.js         # Data formatting utilities
│       │       ├── formatETA()     # Time formatting (s/m/h/d/w/y)
│       │       ├── formatSize()    # Size formatting (B/KB/MB/GB)
│       │       ├── formatSpeed()   # Speed formatting (B/s → GB/s)
│       │       └── getDisplayFormat() # Progress bar template
│       │
│       ├── 📁 loaders/
│       │   └── 📄 index.js         # Loading animations
│       │       └── updateSpinner() # Animated spinner
│       │
│       ├── 📁 logger/
│       │   └── 📄 index.js         # Console logging wrapper
│       │       ├── log()           # Standard output
│       │       ├── error()         # Error output
│       │       └── info()          # Info output
│       │
│       └── 📁 validators/
│           └── 📄 index.js         # Input validation
│               └── isValidMagnetURL() # Magnet link validator
│
├── 📁 downloads/                   # Downloaded files destination
│
├── 📄 package.json                 # Project dependencies & scripts
├── 📄 package-lock.json            # Locked dependency versions
├── 📄 .nvmrc                       # Node version specification (22)
└── 📄 README.md                    # This file
```

### Module Responsibilities

| Module | Purpose | Key Functions |
|--------|---------|---------------|
| **server.js** | Entry point & user interaction | `promptUser()` |
| **client/index.js** | Torrent and direct URL management | `startDownload()`, `startURLDownload()` |
| **formatters** | Data presentation | `formatETA()`, `formatSize()`, `formatSpeed()` |
| **loaders** | Visual feedback | `updateSpinner()` |
| **logger** | Console output | `log()`, `error()`, `info()` |
| **validators** | Input validation | `isValidMagnetURL()`, `isValidDownloadURL()` |

---

## 🚀 Installation

### Prerequisites

- **Node.js**: Version 22.0.0 or higher (LTS recommended)
- **npm**: Comes with Node.js
- **Operating System**: Windows, macOS, or Linux

### Quick Start

1. **Clone or Download the Repository**
   ```bash
   git clone <repository-url>
   cd nodeWT
   ```

2. **Install Dependencies**
   ```bash
   npm install
   ```

3. **Verify Installation**
   ```bash
   node --version  # Should show v22.x.x
   ```

---

## 💻 Usage

### Basic Usage

1. **Start the Application**
   ```bash
   npm start
   ```

2. **Choose a download type**
   ```
   Choose download type:
   1. Magnet URL
   2. Direct URL (multi-part)
   ```

3. **Enter the URL**
   ```
   Please paste the direct download URL:

   https://example.com/file.zip
   ```

4. **Monitor Progress**
   ```
   [████████████████░░░░] 75% | Peers: 12 | D: 750.45 MB @ 2.5 MB/s | T: 1.0 GB | ETA: 2 minutes
   ```

Direct URL downloads are saved to `./downloads`. If the server returns `Accept-Ranges: bytes` and a file size, four parts download concurrently. Servers without range support use a regular streamed download.

### Example Session

```bash
$ npm start

> webtorrent@1.0.0 start
> cross-env NODE_NO_WARNINGS=1 node server.js

Please paste your magnetURI:

magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10

/|\-  Initializing torrent client & verifying data...  -|\/


Torrent is downloading...

Progress

[████████████████████] 100% | Peers: 15 | D: 1.0000 GB @ 3.2 MB/s | T: 1.0000 GB | ETA: 0 seconds

Torrent download finished !
```

### Progress Bar Legend

```
[████████████████░░░░] 75% | Peers: 12 | D: 750.45 MB @ 2.5 MB/s | T: 1.0 GB | ETA: 2 minutes
 └─────┬──────┘        │      │           │            │           │           └─── Time remaining
       │               │      │           │            │           └─────────────── Total file size
       │               │      │           │            └─────────────────────────── Download speed
       │               │      │           └──────────────────────────────────────── Downloaded amount
       │               │      └──────────────────────────────────────────────────── Connected peers
       │               └─────────────────────────────────────────────────────────── Progress percentage
       └─────────────────────────────────────────────────────────────────────────── Visual progress bar
```

---

## 🔧 How It Works

### 1. **Initialization Phase**
```
User Input → Validation → Spinner Animation → WebTorrent Client Init
```

### 2. **Connection Phase**
```
Magnet URI → DHT Lookup → Peer Discovery → Establish Connections
```

### 3. **Download Phase**
```
┌─────────────────────────────────────┐
│  Download Event (triggered by WT)   │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│  Extract Torrent Statistics:        │
│  • downloaded (bytes)               │
│  • length (total bytes)             │
│  • downloadSpeed (bytes/sec)        │
│  • timeRemaining (ms)               │
│  • progress (0-1)                   │
│  • numPeers (count)                 │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│  Format Data:                       │
│  • formatSize(downloaded)           │
│  • formatSize(length)               │
│  • formatSpeed(downloadSpeed)       │
│  • formatETA(timeRemaining)         │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│  Update Progress Bar                │
│  (cli-progress library)             │
└──────────────┬──────────────────────┘
               │
               ▼
┌─────────────────────────────────────┐
│  Check if progress === 1            │
│  Yes → Stop & Exit                  │
│  No  → Wait for next event          │
└─────────────────────────────────────┘
```

### 4. **Completion Phase**
```
Progress 100% → Stop Progress Bar → Log Success → Exit Process
```

---

## ⚙️ Configuration

### Download Location

By default, files are downloaded to `./downloads`. To change this:

````javascript
client.add(magnetURI, { path: './downloads' }, (torrent) => {
    // Change './downloads' to your preferred path
````

### Progress Bar Format

Customize the progress bar display:

````javascript
export const getDisplayFormat = () => `[{bar}] {percentage}% | Peers: {numPeers} | D: {have} @ {speed} | T: {fullSize} | ETA: {estimate}`;
````

### Node.js Version

The project requires Node.js 22+. This is enforced in `package.json`:

````json
"engines": {
  "node": ">=22.0.0"
}
````

---

## 🛠️ Development

### Available Scripts

```bash
# Start the application
npm start

# Run tests (not implemented yet)
npm test
```

### Dependencies

| Package | Version | Purpose |
|---------|---------|---------|
| `webtorrent` | ^2.1.27 | Core torrent client library |
| `cli-progress` | ^3.12.0 | Terminal progress bar |
| `cross-env` | ^7.0.3 | Cross-platform environment variables |

### Adding New Features

1. **New Utility Function**: Add to appropriate `src/utils/` subdirectory
2. **Client Logic**: Modify `src/client/index.js`
3. **Entry Point**: Update `server.js` for CLI changes

---

## 🐛 Troubleshooting

### Common Issues

#### Issue: "Invalid magnet URL"
**Solution**: Ensure your magnet link starts with `magnet:?xt=urn:` and contains a valid info hash.

#### Issue: No peers found
**Solution**:
- Check your internet connection
- Verify the torrent is still being seeded
- Try a different magnet link

#### Issue: Slow download speeds
**Solution**:
- More peers = faster downloads (wait for peer discovery)
- Check your network bandwidth
- Ensure no firewall is blocking connections

#### Issue: Node version error
**Solution**:
```bash
# Check your Node.js version
node --version

# If < 22.0.0, upgrade Node.js
# Using nvm (recommended):
nvm install 22
nvm use 22
```

### Debug Mode

To see all Node.js warnings (disabled by default):

```bash
# Windows
set NODE_NO_WARNINGS=0 && node server.js

# Linux/macOS
NODE_NO_WARNINGS=0 node server.js
```

---

## 🤝 Contributing

Contributions are welcome! Here's how you can help:

1. **Fork the repository**
2. **Create a feature branch**: `git checkout -b feature/amazing-feature`
3. **Commit your changes**: `git commit -m 'Add amazing feature'`
4. **Push to the branch**: `git push origin feature/amazing-feature`
5. **Open a Pull Request**

### Code Style

- Use ES6+ features
- Follow existing code structure
- Add comments for complex logic
- Update README for new features

---

## 📄 License

This project is licensed under the MIT License.

---

## 🙏 Acknowledgments

- **WebTorrent**: For the amazing torrent library
- **cli-progress**: For the beautiful progress bars
- **Node.js Community**: For continuous support

---

## 📞 Support

If you encounter any issues or have questions:

1. Check the [Troubleshooting](#-troubleshooting) section
2. Review existing issues on GitHub
3. Create a new issue with detailed information

---

<div align="center">

**Made with ❤️ by danted4**

⭐ Star this repo if you find it useful!

</div>
