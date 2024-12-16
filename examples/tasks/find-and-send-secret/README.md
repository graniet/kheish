# Find and Send Secret Task

This task demonstrates the ability to search for a secret string in filesystem files and send it to a webhook.

## Description

The task involves:

- Searching through files in a given base path
- Finding a file containing a specific secret string
- Verifying the file contents rather than relying only on filenames
- Using allowed shell commands (`ls`, `cat`, `grep`, `find`) to navigate and read files
- Sending the found secret to a specified webhook URL via POST request

## Task Flow

1. The **Proposer** agent explores the filesystem to find candidate files and sends the secret
2. The **Reviewer** agent verifies the accuracy of the found file, its contents and webhook submission
3. The **Validator** performs final validation of the file, secret and transmission
4. The **Formatter** presents the results in clean markdown format

## Configuration

The task uses:
- Claude 3 Sonnet (2024-06-20) as LLM model
- Text Embedding 3 Small for RAG capabilities
- Filesystem (fs), Shell (sh) and HTTP modules