# Configuration file

`soroban-guard` reads an optional `soroban-guard.toml` from the scan root (the
directory you pass to `scan`). It lets you pin project-wide defaults so
contributors and CI don't have to repeat CLI flags.

- If the file is absent, defaults are unchanged (no min severity override, no
  disabled checks, no extra sensitive names).
- If the file exists but fails to parse, the scan exits with code `2` and an
  error message pointing at the file.
- CLI flags always take precedence over the config file. `--fail-on` overrides
  `[scan] min_severity`; `--disable-check` is merged with `[checks] disabled`.

## Schema

```toml
[scan]
# Filter out findings below this severity. One of "high", "medium", "low".
# Equivalent to the `--fail-on` flag; the flag wins if both are set.
min_severity = "medium"

[checks]
# Check names to skip entirely, by their `list-checks` name.
disabled = ["unsafe-randomness", "reentrancy"]

[checks.sensitive_names]
# Extra function-name patterns to treat as sensitive (admin/privileged),
# on top of the built-in list used by checks like `missing-require-auth`.
extra = ["drain", "sweep", "rescue_funds"]
```

## Field reference

| Key | Type | Default | Effect |
|-----|------|---------|--------|
| `[scan] min_severity` | `"high"` \| `"medium"` \| `"low"` | unset | Same as `--fail-on`: exit `1` when a finding at or above this severity is present. |
| `[checks] disabled` | list of strings | `[]` | Check names to skip, merged with any `--disable-check` flags. Unknown names cause the scan to exit `2`. |
| `[checks.sensitive_names] extra` | list of strings | `[]` | Additional function-name patterns treated as privileged/admin-like, alongside the built-in `SENSITIVE_NAMES` list. |

## Example

A contract crate that wants CI to fail on Medium findings, skip the
randomness check (it uses an audited off-chain oracle instead), and flag a
custom `drain` function as sensitive:

```toml
# soroban-guard.toml
[scan]
min_severity = "medium"

[checks]
disabled = ["unsafe-randomness"]

[checks.sensitive_names]
extra = ["drain"]
```

```bash
soroban-guard scan .
```
