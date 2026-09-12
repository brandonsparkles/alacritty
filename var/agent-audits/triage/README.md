# Fleet Triage (evergreen)

This is the active work queue for *all* known agent-audit findings
in this repo. Per-run dirs under shared-runtime `var/agent-audits/<run>/` remain as
archive evidence of the runs that surfaced each finding.

- `index.ndjson` is the evergreen lifecycle overlay keyed by `finding_id`.
- Global rows persist `rule_id` from canonical findings so rule-level noise/precision analysis can query the evergreen index directly.
- `shards/*.ndjson` are bounded work queues. Give fix/validation/verification agents one shard, never the full `index.ndjson`.
- Lifecycle: `needs-validation` -> `validated` -> `in-progress` -> `fixed` -> `verified`.
- Status precedence on collision: `verified|false-positive|stale` > `fixed` > `in-progress` > `validated` > `deferred|feature-work` > `needs-validation`. New sightings never regress an existing row's status.
- Shards split after 30 findings when a group is large.

## Promote one run

```bash
python3 scripts/analysis/agent-audit-promote.py var/agent-audits/<run>
```

## Regenerate shards without ingesting a run

```bash
python3 scripts/analysis/agent-audit-promote.py --regenerate-shards-only
```

## Lifecycle updates

Update one finding's status (per-run script writes to the per-run overlay,
promote it forward by re-running promote on its source run):

```bash
python3 scripts/analysis/agent-php-audit-triage.py var/agent-audits/<run> \
    --set-status <finding_id> --status validated --validated-now
python3 scripts/analysis/agent-audit-promote.py var/agent-audits/<run>
```

Or edit the global row directly (advanced) by writing JSON to
`triage/index.ndjson` keyed by finding_id, then run --regenerate-shards-only.
