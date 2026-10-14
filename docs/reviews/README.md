# Design reviews

These reviews were written by AI agents (Claude) during development. Each one
was asked to check the project against the Scion article or to argue against
it. They are kept as a record of the critiques and of what changed because of
them. They are not independent third-party reviews.

- The project was called **Flux** when they were written. It was renamed
  **Wren**, partly because of review 07's finding that the name collided
  with FluxCD and with crates already on crates.io.
- Paths such as `scratchpad/...` refer to local working directories that are
  not part of this repository.
- Numbers come from the experiments described in each review. Where they
  matter, they have been reproduced in [benchmarks.md](../benchmarks.md) with
  scripts under [bench/](../../bench).

| Review | Question |
|---|---|
| [01](01-principles-conformance.md) | Does the design follow the article's principles? |
| [02](02-architecture-fidelity.md) | Does the architecture match Scion's? |
| [03](03-strategy-and-claims.md) | Are the public claims and benchmarks fair? |
| [04](04-positioning-vs-intent.md) | Does the positioning match what the article asks for? |
| [05](05-protocol-as-git-server-redesign.md) | Is the protocol a real redesign of git's server side, compared with git protocol v2? |
| [06](06-agent-scale-walls.md) | Which walls do agent-heavy teams hit first, and which does this project address? |
| [07](07-devils-advocate.md) | Devil's advocate: should this exist, and what would critics attack? |
| [08](08-synthesis.md) | What to keep, change and cut |

Most of the reviews are in Chinese.
