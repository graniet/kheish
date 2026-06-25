---
description: Read, search, and create notes in the Obsidian vault.
---

## Kheish Compatibility

This skill is repo-local and stays inactive until explicitly activated.

When the original instructions refer to legacy tool names, use these Kheish mappings:

- `terminal` => `bash`
- `web_extract` => `web_fetch`, plus `web_search` when discovery is needed
- `search_files` => `grep_search` and `glob_search`
- `browser_*` tools require a browser-capable surfaced tool or MCP; if none is available, use the closest available surface and say so explicitly

When the instructions mention local helper files, resolve them from `${KHEISH_SKILL_DIR}`.

# Obsidian Vault

**Location:** Set via `OBSIDIAN_VAULT_PATH` environment variable (e.g. in `~/.kheish/.env`).

If unset, defaults to `~/Documents/Obsidian Vault`.

Note: Vault paths may contain spaces - always quote them.

## Read a note

```bash
VAULT="${OBSIDIAN_VAULT_PATH:-$HOME/Documents/Obsidian Vault}"
cat "$VAULT/Note Name.md"
```

## List notes

```bash
VAULT="${OBSIDIAN_VAULT_PATH:-$HOME/Documents/Obsidian Vault}"

# All notes
find "$VAULT" -name "*.md" -type f

# In a specific folder
ls "$VAULT/Subfolder/"
```

## Search

```bash
VAULT="${OBSIDIAN_VAULT_PATH:-$HOME/Documents/Obsidian Vault}"

# By filename
find "$VAULT" -name "*.md" -iname "*keyword*"

# By content
grep -rli "keyword" "$VAULT" --include="*.md"
```

## Create a note

```bash
VAULT="${OBSIDIAN_VAULT_PATH:-$HOME/Documents/Obsidian Vault}"
cat > "$VAULT/New Note.md" << 'ENDNOTE'
# Title

Content here.
ENDNOTE
```

## Append to a note

```bash
VAULT="${OBSIDIAN_VAULT_PATH:-$HOME/Documents/Obsidian Vault}"
echo "
New content here." >> "$VAULT/Existing Note.md"
```

## Wikilinks

Obsidian links notes with `[[Note Name]]` syntax. When creating notes, use these to link related content.
