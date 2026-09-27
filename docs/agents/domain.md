# Domain Docs

How engineering skills should consume this repo's domain documentation when exploring the codebase.

## Before exploring, read these

- **`.agent/CONTEXT.md`**: the single domain context for this repository.
- **`docs/adr/`**: read ADRs that touch the area you are about to work in.

If an ADR directory does not exist, proceed silently. The `/domain-modeling` skill creates ADRs lazily when terms or decisions are resolved.

## File structure

```text
/
├── .agent/
│   └── CONTEXT.md
├── docs/
│   └── adr/
└── src/
```

## Use the glossary's vocabulary

When naming a domain concept in an issue title, refactor proposal, hypothesis, or test name, use the terms defined in `.agent/CONTEXT.md`. Do not drift to synonyms the glossary explicitly avoids.

If a needed concept is absent, reconsider whether the project already has a suitable term. Otherwise, note the gap for `/domain-modeling`.

## Flag ADR conflicts

If output contradicts an existing ADR, surface it explicitly rather than silently overriding it.
