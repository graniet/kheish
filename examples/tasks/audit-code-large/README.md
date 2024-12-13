# Large Codebase Security Audit Task

This task demonstrates the ability to perform a thorough security audit of a large codebase using minimal RAG capabilities.

## Description

The task involves:

- Recursively exploring and indexing an entire codebase using RAG
- Focusing on critical and high severity vulnerabilities only
- Analyzing data flows and user input propagation
- Providing concrete evidence with code snippets for each vulnerability
- Using RAG queries to retrieve relevant code segments
- Following OWASP Top 50 security guidelines

## Task Flow

1. **Proposer** agent explores the codebase and identifies vulnerabilities
2. **Reviewer** agent verifies the findings and evidence
3. **Validator** performs final validation of the security audit
4. **Formatter** presents the results in a comprehensive markdown report

## Configuration

The task uses:
- Claude 3 Sonnet (2024-06-20) as the LLM model
- Text Embedding 3 Small for RAG capabilities
- Filesystem (fs) and RAG modules

## Output

Results are exported to `exports/audit-large-report.md` in markdown format, including:
- A summary of findings
- Detailed vulnerability descriptions
- Code evidence from RAG queries
- Severity levels and impact analysis

For full configuration details, see the [task.yaml](task.yaml) file.
