# Kheish

<p align="center">
  <img src="docs/logo.png" alt="Kheish Logo" width="250">
</p>

**Kheish** is an open-source, multi-role **agent** designed for complex tasks that require structured, step-by-step collaboration with Large Language Models (LLMs). Rather than a simple orchestrator, Kheish itself acts as an **intelligent agent** that can request modules on demand, integrate user feedback, switch between specialized roles (Proposer, Reviewer, Validator, Formatter, etc.), and ultimately deliver a refined result. By harnessing multiple “sub-agents” (roles) within one framework, Kheish tackles tasks like security audits, file searches, RAG-based exploration, and more.

## Highlights of Kheish as an **Agent**

- **Adaptive Role Switching**  
  Kheish functions as a single agent with multiple internal roles:
  - **Proposer**: Generates or updates proposals based on user input and context.  
  - **Reviewer**: Critically evaluates proposals, identifying flaws or requesting improvements.  
  - **Validator**: Final gatekeeper ensuring correctness and completeness.  
  - **Formatter**: Takes a validated solution and converts it into a final presentation format (Markdown, etc.).  
  These roles can be enabled or disabled depending on the task definition in your YAML file.

- **On-Demand Module Requests**  
  As an agent, Kheish can spontaneously invoke modules if it needs more information or functionality. Modules include:  
  - **Filesystem (`fs`)**: Reading files chunk by chunk, indexing them in RAG.  
  - **Shell (`sh`)**: Running limited shell commands with sandboxed allowances.  
  - **RAG (`rag`)**: Storing and retrieving large amounts of text via embeddings, enabling chunk-based queries.  
  - **SSH (`ssh`)**: Secure remote commands.  
  - **Memories (`memories`)**: Storing or recalling data outside the immediate LLM context (long-term memory).

- **Feedback & Iteration**  
  In many tasks, Kheish re-checks and revises its own proposals. For example:
  1. **Proposer** suggests a solution.
  2. **Reviewer** critiques and possibly requests changes.
  3. **Proposer** refines based on feedback.
  4. **Validator** delivers final approval or requests more fixes.  
  This iterative approach provides an agent that grows the solution’s quality step by step.

- **Retrieval-Augmented Generation (RAG)**  
  For large codebases or multi-file contexts, Kheish indexes data in a vector store. It can retrieve relevant snippets later without stuffing the entire text into a single LLM prompt. This agent-based RAG integration reduces token usage and scales to bigger projects.

- **Single Agent, Many Tasks**  
  Kheish can handle parallel or serial tasks by defining separate YAML configurations or combining them into a single multi-step scenario. Each role or module request is orchestrated **internally** by Kheish’s logic—no external orchestrator needed.

## Example Tasks

- **`audit-code`**: A thorough security audit of a codebase, identifying potential vulnerabilities via multi-step agent roles.  
- **`hf-secret-finder`**: Requests the Hugging Face API, clones the repositories, and uses `trufflehog` (via the `sh` module) to detect secrets.  
- **`find-in-file`**: Searches for a secret across multiple files, chunk-reading them with `fs`.  
- **`weather-blog-post`**: Fetches live weather data (via `web` or a custom module) and writes a humorous blog post about it.

## How Kheish Works

1. **Reads a YAML Configuration**  
   Includes the agent roles, modules, the workflow of steps, and final output instructions.
2. **Builds an Agent**  
   Kheish loads the roles (Proposer, Reviewer, etc.) and hooks in the modules for possible requests.  
3. **Executes Steps Internally**  
   The agent:
   - Gathers context (files, text).
   - Generates or refines a solution (`Proposer`).
   - Seeks feedback (`Reviewer`) if needed.
   - Validates correctness (`Validator`).
   - Formats the final result (`Formatter`).
4. **Optional RAG Integration**  
   If large data is encountered, the agent chunk-indexes it into a vector store, retrieving relevant pieces via semantic queries.
5. **Output**  
   Once validated, Kheish saves or exports the final solution. If further feedback is provided, it can loop back into revision mode automatically.

## Installation & Usage

1. **Clone the Repository**  
   ```bash
   git clone https://github.com/yourusername/kheish.git
   cd kheish
   ```
2. **Install Dependencies**  
   - Rust toolchain (latest stable).  
   - `OPENAI_API_KEY` or other relevant environment variables for your chosen LLM provider.
3. **Build**  
   ```bash
   cargo build --release
   ```
4. **Run a Task**  
   ```bash
   ./target/release/kheish --task-config examples/tasks/audit-code.yaml
   ```

## Contributing

Contributions to Kheish are welcome! Feel free to open issues or submit pull requests on [GitHub](https://github.com/graniet/kheish).

## License

Licensed under [Apache 2.0](LICENSE).