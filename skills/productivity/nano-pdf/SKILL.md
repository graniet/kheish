---
description: Edit PDFs with natural-language instructions using the nano-pdf CLI.
  Modify text, fix typos, update titles, and make content changes to specific pages
  without manual editing.
version: 1.0.0
author: community
license: MIT
metadata:
  catalog:
    tags:
    - PDF
    - Documents
    - Editing
    - NLP
    - Productivity
    homepage: https://pypi.org/project/nano-pdf/
---

## Kheish Compatibility

This skill is repo-local and stays inactive until explicitly activated.

When the original instructions refer to legacy tool names, use these Kheish mappings:

- `terminal` => `bash`
- `web_extract` => `web_fetch`, plus `web_search` when discovery is needed
- `search_files` => `grep_search` and `glob_search`
- `browser_*` tools require a browser-capable surfaced tool or MCP; if none is available, use the closest available surface and say so explicitly

When the instructions mention local helper files, resolve them from `${KHEISH_SKILL_DIR}`.

# nano-pdf

Edit PDFs using natural-language instructions. Point it at a page and describe what to change.

## Prerequisites

```bash
# Install with uv (recommended — already available in Kheish)
uv pip install nano-pdf

# Or with pip
pip install nano-pdf
```

## Usage

```bash
nano-pdf edit <file.pdf> <page_number> "<instruction>"
```

## Examples

```bash
# Change a title on page 1
nano-pdf edit deck.pdf 1 "Change the title to 'Q3 Results' and fix the typo in the subtitle"

# Update a date on a specific page
nano-pdf edit report.pdf 3 "Update the date from January to February 2026"

# Fix content
nano-pdf edit contract.pdf 2 "Change the client name from 'Acme Corp' to 'Acme Industries'"
```

## Notes

- Page numbers may be 0-based or 1-based depending on version — if the edit hits the wrong page, retry with ±1
- Always verify the output PDF after editing (use `read_file` to check file size, or open it)
- The tool uses an LLM under the hood — requires an API key (check `nano-pdf --help` for config)
- Works well for text changes; complex layout modifications may need a different approach
