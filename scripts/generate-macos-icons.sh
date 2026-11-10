#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
swift scripts/prepare-macos-icons.swift resources/app-icons/layout.json
python3 resources/app-icons/generate.py --mac-small resources/app-icons/AppIcon.iconset
swift scripts/prepare-macos-icons.swift resources/app-icons/layout.json --finish-small
swift scripts/macos-icon.swift build resources/app-icons/AppIcon.iconset src-tauri/icons Queryfolio
