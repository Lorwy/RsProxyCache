# Issue tracker

## Specs and tickets

- Store each feature spec at `.scratch/<feature-slug>/spec.md`.
- Store each implementation ticket in its own file at `.scratch/<feature-slug>/issues/<NN>-<slug>.md`. Number tickets from `01`.
- Record triage in a `Status:` line near the top of the ticket using the vocabulary in [Triage labels](triage-labels.md).
- Append discussion at the bottom under `## Comments`.

To publish a spec or ticket, create its file at the appropriate path, creating parent directories as needed. To fetch a ticket, read its referenced path or resolve its number within the relevant feature directory.

## Wayfinding

- Keep the map at `.scratch/<effort>/map.md` with Notes, Decisions-so-far, and Fog sections.
- Give each child ticket its own numbered file under `.scratch/<effort>/issues/`. Put the question in the body and record its `Type:` as `research`, `prototype`, `grilling`, or `task`.
- Record dependencies as `Blocked by: NN, NN`. A ticket is unblocked when all listed dependencies are resolved.
- Pick the lowest-numbered open, unblocked, unclaimed ticket. Set `Status: claimed` and save before starting work.
- To resolve the ticket, append the answer under `## Answer`, set `Status: resolved`, and add a short summary and link to Decisions-so-far in the map.
