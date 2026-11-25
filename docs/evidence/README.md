# Evidence bundles

These files are sanitized records of controlled **synthetic** acceptance runs of
the real dual-account route. They exist so the verification can be reviewed
without exposing credentials, user paths, prompts, or provider payloads.

## What a bundle contains

- task/route identifiers and the approval SHA-256 that bound the run;
- tool basenames, model IDs, usage counters and billing attribution fields;
- stage timeline (implement / verify / review), event kinds and states;
- patch and receipt digests;
- the redaction policy applied.

## What these files are not

- They are not long-term billing evidence, quota evidence, or a production
  readiness claim.
- They do not cover failure matrices beyond the recorded attempt.
- `charge_attribution: unverified` for MiniMax is intentional: the provider does
  not return reliable debit attribution for this route.

## Regenerating

```sh
relayctl="target/release/relayctl"
python3 packaging/qa/live-project-acceptance.py prepare "$relayctl" <claude> <opencode>
# run once with the two dedicated keys piped via stdin, then:
python3 packaging/qa/evidence-bundle.py <sample-dir> docs/evidence/<name>.json
```

The script reads only `evidence.json`, `sample.json`, and the sample's SQLite
database. Absolute paths collapse to basenames; unexpected free-text reasons are
replaced with `<redacted>`; credentials, prompts and provider bodies are never
copied. It refuses to write a bundle that still matches common secret/path
patterns.

## Recorded runs

| Bundle | Version | Result |
| --- | --- | --- |
| `synthetic-dual-account-0.1.26-task_eec669777e8b.json` | 0.1.26 | completed; MiniMax CN implement + OpenCode Go review, Go balance charge 0 |
| `synthetic-dual-account-0.1.27-task_1597b5dd2f69.json` | 0.1.27 | completed; same route after the retry/fallback work |
