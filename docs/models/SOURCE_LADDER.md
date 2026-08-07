# Model source and accuracy ladder

Every selected model carries one canonical `ModelSource` record through the
evidence spine. It is present in `hauksbee models resolve --json`, `hauksbee
run --json`, `hauksbee-ci run --json`, human evidence, and the Checks UI. The
browser and CLI do not reconstruct provenance from filenames.

## Selection policy

Semantic source tier is evaluated before storage-layer priority and match
specificity:

| tier | meaning |
|---|---|
| `user-model` | explicit user override or a standalone user-supplied SPICE card |
| `vendor-spice` | vendor pack with a nonblank declared licence |
| `curated-pack` | reviewed hand-written installed pack |
| `curated-library` | embedded reviewed model library |
| `datasheet-derived` | extracted draft, including a datasheet-extracted pack |
| `interval-model` | a deliberately bounded model whose finite interval is the claim |
| `estimated-fallback` | generic/engine fallback with no part-specific accuracy claim |
| `open` | no model; the component remains electrically open |

The explicit user override is first because it is the escape hatch. Without
one, a curated or licensed source cannot be silently displaced by a newly
extracted draft merely because the draft lives in a higher-priority directory.
Storage layer still breaks ties inside one semantic tier, and match specificity
still breaks ties inside one layer.

Installed packs declare `hand-written`, `datasheet-extracted`, or `vendor`
provenance. A vendor pack with a blank licence is rejected before installation;
Hauksbee does not promote material with no declared licence to the vendor rung.
This is a syntactic provenance gate, not legal verification of the declared
licence or permission for a particular use.

## Accuracy is data, including unknown accuracy

`ModelSource.validation` is one of `unvalidated`, `physical-bounds-only`,
`datasheet-curves`, or `vendor-qualified`. Range checking is not presented as
curve validation.

`ModelSource.uncertainty[]` is a tagged value:

```toml
[models.source]
tier = "datasheet-derived"
validation = "physical-bounds-only"

[[models.source.uncertainty]]
status = "interval"
parameter = "vout"
low = 3.201
high = 3.399
unit = "V"
kind = "specification-limits"
basis = "datasheet output-voltage limits"
```

If no defensible finite interval exists, the record is explicit:

```toml
[[models.source.uncertainty]]
status = "unknown"
parameter = "model"
reason = "datasheet publishes no validated model error interval"
```

Finite intervals are validated for finite ordered bounds before publication.
Their `kind` distinguishes `specification-limits` and `empirical-error` from
non-guaranteed `typical-range` and `estimated-range`. A min/typ row with no
published maximum remains `unknown`; a model clamp is not a datasheet bound.
Hauksbee never turns `unknown` into an estimated percentage. These intervals
are provenance/error-budget evidence; they do not claim that the current scalar
solver has propagated every model interval through a transient result.

## Fail-closed validation switches

Use the resolution surface before a high-accuracy run:

```bash
hauksbee models resolve board.kicad_pcb --min-model-tier curated-library
hauksbee models resolve board.kicad_pcb --min-model-validation datasheet-curves
hauksbee models resolve board.kicad_pcb --require-model-intervals --json
```

If any component is open, below the requested source or validation rung, or
lacks a validated two-sided specification/empirical interval, the command
refuses with exit 3.
Typical and estimated ranges remain visible but cannot satisfy this gate. JSON reports
`status: "invalid_for_analysis"`, `reason: "model_accuracy_insufficient"`, and
one reference-qualified refusal per affected component. This is a validation
refusal, not a failed electrical assertion.
