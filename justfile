default:
    just --list

# Run the CLI locally.
dev *args:
    cargo r -- {{ args }}

# Build release artifacts with cargo-dist.
dist-build *args:
    dist build {{ args }}

preview *args:
    ./target/release/cftun {{ args }}

dpreview *args:
    ./target/debug/cftun {{ args }}

sync_readme:
    cp README.md npm/README.md

# Release: bump versions, create release commit, and create a git tag.

# Usage: just tag [patch|minor|major]
tag bump="":
    sh scripts/tag_and_release.sh {{ bump }}
