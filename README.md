# Quick-CLI 🚀

Quick-CLI is a terminal tool for managing QuickEMU VMs.

## Features 🛠️
- List available QuickEMU VMs
- Start, stop, and connect to VMs with ease
- Detect running VMs and show connection status
- Support for Remmina and SPICE connections
- Simple keyboard navigation

## Installation ⚙️
Make sure you have Rust installed. Then, clone the repository and build the project:

```sh
git clone https://github.com/mmrmagno/quick-cli.git
cd quick-cli
cargo build --release
```

## Usage 🖥️
Run Quick-CLI with:

```sh
./target/release/quick-cli
```

### Controls:
- `↑ / ↓` or `j / k` - Navigate VMs
- `Enter` - Start & Connect VM
- `r` - Start VM
- `c` - Connect to running VM
- `s` - Stop VM
- `q` - Quit

## Requirements 🛠️
- Rust
- QuickEMU installed

## License 📝
This project is licensed under the [Apache License 2.0](LICENSE).
