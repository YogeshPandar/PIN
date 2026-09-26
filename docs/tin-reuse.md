# What PIN can take from TIN

PlanetScale publishes TIN's architecture and language documentation. Its public
Lead repository explicitly refers to a private TIN source tree. Lead exposes the
compatible parser, tokenizer, scoring and highlighting surface, but its index
stores no search data and emits all heap pages as candidates. Copying that scan
implementation would undo PIN's performance work.

Reference checkout inspected for this iteration:
`planetscale/lead@bd95c7e51b6afce81396790852ee2f2c169570ad`.
Its workspace declares AGPL-3.0-or-later; PIN declares Apache-2.0 OR MIT. This
iteration imports no Lead code or dependency. It uses published behavior as a
feature reference and implements its own reader. Lead is useful as an external
semantic oracle, not a performance baseline.

Published mechanisms guiding implementation:

- Separate membership, frequency and positions so each consumer fetches only
  the information it needs.
- Use CTID/page-coordinate grouping and positional iterators to eliminate work.
- Use coherent statistics and conservative score bounds for native BM25 top-k.
- Preserve PostgreSQL visibility, lifecycle identity and publication discipline.

The current prefix-witness reader implements a bounded piece of selective reads:
when a phrase is proven in the first fragment, it skips the physical tail and
stops decoding occurrences. Its incomplete-prefix probe has a fixed work budget.
This is our implementation over PIN's PD02 layout, not a claim about TIN's codec.
The remaining packed term/position-block layout must allow efficient negative
proofs and late matches, which still fall back to full legacy payload access.

Sources:

- [PlanetScale search-engine architecture](https://planetscale.com/blog/anatomy-of-a-postgres-search-engine)
- [Lead at the inspected revision](https://github.com/planetscale/lead/tree/bd95c7e51b6afce81396790852ee2f2c169570ad)
- [Why Lead omits the performance engine](https://planetscale.com/blog/introducing-lead)
- [PostgreSQL index scanning](https://www.postgresql.org/docs/18/index-scanning.html)
