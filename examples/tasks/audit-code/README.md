# Code Security Audit Task

This task demonstrates an automated security audit workflow for code repositories using LLM agents.

## Demo

Watch the task execution in action:

[![asciicast](https://asciinema.org/a/DOY7HqoLuiXTvKvYZnwrVQ0ov.svg)](https://asciinema.org/a/DOY7HqoLuiXTvKvYZnwrVQ0ov)

## Description

The task performs a comprehensive security audit by:

- Scanning all files in the target repository
- Identifying potential security vulnerabilities and risks
- Analyzing code for best practices violations
- Providing severity ratings and detailed explanations
- Generating a well-formatted markdown report

## Task Flow

1. **Proposer** agent performs initial security analysis of all files
2. **Reviewer** agent validates findings and requests revisions if needed
3. **Validator** performs final validation of the audit
4. **Formatter** creates a clean markdown report

## Configuration

The task uses:
- GPT-4 as the LLM model
- Text Embedding 3 Small for RAG capabilities
- Filesystem (fs) and RAG modules for code analysis

## Output

Results are exported to `exports/audit-report.md` in markdown format, including:
- Vulnerability severity levels
- Detailed explanations
- File and line references
- Code snippets where relevant

For full configuration details, see the [audit-code.yaml](audit-code.yaml) file.
