# Find in File Task

This task demonstrates the ability to search for a secret string within files in a filesystem.

## Demo

Watch the task execution in action:

[![asciicast](https://asciinema.org/a/7P0WGDBTFEiKL2tagCI4FUZYR.svg)](https://asciinema.org/a/7P0WGDBTFEiKL2tagCI4FUZYR)

## Description

The task involves:

- Searching through files in a given base path
- Finding a file containing a specific secret string
- Verifying the file contents rather than just relying on filenames
- Using allowed shell commands (`ls`, `cat`, `echo`, `pwd`) to navigate and read files

## Task Flow

1. **Proposer** agent explores the filesystem to find candidate files
2. **Reviewer** agent verifies the correctness of the found file and its contents
3. **Validator** performs final validation of the file and secret
4. **Formatter** presents the results in a clean markdown format

## Configuration

The task uses:
- Claude 3 Sonnet (2024-06-20) as the LLM model
- Text Embedding 3 Small for RAG capabilities
- Filesystem (fs), RAG, and Shell (sh) modules

## Output

Results are exported to `exports/find-in-file.md` in markdown format.

For full configuration details, see the [find-in-file.yaml](find-in-file.yaml) file.
