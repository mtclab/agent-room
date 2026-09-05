#!/usr/bin/env bash
# Print one version's section of CHANGELOG.md, without its heading.
#
#   scripts/changelog-section.sh 1.0.0-rc.5            # the section for that version
#   scripts/changelog-section.sh "$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml)"
#
# The release workflow feeds the output to `gh release create --notes-file`,
# so the GitHub Release page says what changed instead of listing pull-request
# titles. tests/changelog.rs runs the same script against the version in
# Cargo.toml on every `make gate`, so a version with no section fails the gate
# before it can fail the release.
#
# Exit codes: 0 with the section on stdout; 1 if the version has no section or
# the section is empty; 2 on usage or a missing file.
set -euo pipefail

version="${1:-}"
file="${2:-$(dirname "$0")/../CHANGELOG.md}"

if [ -z "$version" ]; then
    echo "usage: $0 <version> [CHANGELOG.md]" >&2
    exit 2
fi
if [ ! -f "$file" ]; then
    echo "no changelog at $file" >&2
    exit 2
fi

# A section starts at `## [<version>]` and ends at the next `## ` heading or at
# the link-reference block at the bottom (`[<version>]: <url>` lines).
section="$(awk -v want="## [$version]" '
    /^## / { if (found) exit; if (index($0, want) == 1) { found = 1; next } }
    /^\[[^]]+\]: / { if (found) exit }
    found { print }
' "$file")"

# Trim leading and trailing blank lines so the caller sees content or nothing.
section="$(printf '%s\n' "$section" | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}' | sed '/./,$!d')"

if [ -z "$section" ]; then
    echo "CHANGELOG.md has no section for ${version}: add '## [${version}] - YYYY-MM-DD' with what changed" >&2
    exit 1
fi

printf '%s\n' "$section"
