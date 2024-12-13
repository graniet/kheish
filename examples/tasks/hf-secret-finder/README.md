# Hugging Face Secret Finder Task

This task demonstrates the ability to scan Hugging Face repositories for potential secrets using trufflehog.

## Demo

Watch the task execution in action:

[![asciicast](https://asciinema.org/a/Q6pA27hBo9x1wBgZQjZpSJpvX.svg)](https://asciinema.org/a/Q6pA27hBo9x1wBgZQjZpSJpvX)

Note: For demonstration purposes, the task is limited to scanning only 3 repositories. 

## Description

The task involves:

- Fetching repository information from the Hugging Face API
- Cloning repositories into a local directory
- Scanning each repository using trufflehog to detect potential secrets
- Storing and managing findings using the memories module
- Generating a comprehensive report of discovered secrets

## Prerequisites

This task requires [trufflehog](https://github.com/trufflesecurity/trufflehog) to be installed on your system.

## Task Flow

1. **Proposer** agent:
   - Fetches repositories from Hugging Face API
   - Stores repository URLs in memories
   - Clones repositories and runs trufflehog scans
   - Records discovered secrets in memories
   - Produces initial findings report
2. **Reviewer** agent verifies:
   - Repository data was properly stored
   - Scans were completed without duplication
   - All secrets were recorded in memories
   - Report accuracy and completeness
3. **Formatter** agent creates structured markdown report with:
   - Title and table of contents
   - Per-repository sections with findings

## Configuration

The task uses:
- Claude 3 Sonnet (2024-06-20) as the LLM model
- Text Embedding 3 Small for RAG capabilities
- Memories module for state management
- Shell module with allowed commands: curl, git, trufflehog

## Output

Results are exported to `exports/hf-secret-finder.md` in markdown format.

For full configuration details, see the [task.yaml](task.yaml) file.

