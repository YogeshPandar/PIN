# G1 reference semantics

The pure engine is not a PostgreSQL AM or a visibility implementation. G0 host
contracts and APIs are unchanged. All formats remain experimental.

## Frozen analysis profile 1

Unicode 16.0.0. Apply NFC, Unicode default **simple** case folding, NFC again,
then UAX #29 word segmentation. Keep segments containing a Unicode alphabetic or
numeric character, as specified by `unicode_words`. No stemming, stopwords,
locale-specific rules or compatibility normalization is implicit. This is not
PostgreSQL `tsvector` compatibility and not full Unicode case folding.

Use exact dependencies `unicode-normalization 0.1.24`, `unicode-segmentation
1.12.0`, and `unicode-case-mapping 1.0.0`. Their exported Unicode versions are
checked against 16.0.0. A future dependency or table change requires explicit
semantic review, not a silent analyzer replacement. For example, composed and
decomposed café match, Greek sigma forms fold together, capital sharp S folds to
sharp S, but sharp S does not expand to `ss`. Fullwidth text is not ASCII text.

Positions start at zero and advance once per retained word. Document length is
the exact retained token count. Empty non-null text is a valid document with no
terms. SQL NULL is represented separately and is not part of the index universe.

Input bytes, normalized bytes, token count and per-term bytes have explicit
limits. Pin-owned vector capacities are charged to a checked budget with
fallible reservation. Unicode normalization's internal buffers are separately
bounded by the finite input-byte limit, but are not included in the reported
Pin-owned capacity count. This is not a process-wide hard memory limit or an OOM
recovery guarantee. Full allocator/scratch accounting is an outstanding G1
acceptance item before host integration.

## Query syntax version 1

Explicit uppercase `NOT`, `AND`, `OR`; precedence is NOT, AND, OR. Parentheses
override precedence. Binary operators associate left. Adjacent operands are an
error; there is no implicit AND. Lowercase operator spellings are ordinary words.
Whitespace outside phrases is ASCII whitespace. Unquoted literals must analyze
to exactly one token. Only a terminal `*` denotes a term prefix; no fuzzy, regex,
infix wildcard or arbitrary escape syntax is accepted.

Double quotes enclose a phrase. Inside quotes only `\"` and `\\` are escapes.
The same analyzer processes every literal. Empty input or a phrase with no words
matches nothing; its negation matches the non-null complete/live universe.
Repeated phrase words require distinct consecutive occurrences. Repeated Boolean
clauses do not duplicate documents. Unknown terms are false.

The parser uses flat postorder nodes and bounded explicit stacks; parsing,
evaluation and dropping a query do not recursively walk user-controlled syntax.
Node count, term count, nesting, semantic tree depth, text bytes, term bytes and
Pin-owned capacity are checked. Errors never return a partial query.

`Query::source` preserves the original text. Kind 5 of the record envelope stores
`profile:u32, syntax_version:u16, reserved:u16, source_bytes:u32, utf8_source`.
Decode validates the envelope, profile, syntax version, reserved bytes and UTF-8,
then reparses under current explicit limits. No native Rust AST layout is stored.

## Independent document oracle

`oracle::matches` scans analyzed tokens directly, evaluates flat Boolean nodes,
and compares phrase windows without dictionaries or postings. NULL stays `None`
through `matches_nullable`. Work and capacity limits produce errors, never
incomplete exact results. These functions are pure predicate oracles, not SQL
bindings or snapshot checks.
