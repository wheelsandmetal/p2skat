---
name: verify
description: Prove a p2skat change works before calling it done, committing, or opening a PR. Runs scripts/verify.sh, which picks the cargo, Wasm, node, conformance and browser smoke checks from the changed files, and says what the script cannot cover (native TUI play, Tor, the router). Use for "verify", "does it work", "run the tests", or at the end of any code change in this repo.
---

# Verify a p2skat change

```sh
scripts/verify.sh --list     # which gates the diff selects, and the file that chose each
scripts/verify.sh            # run them; logs in target/verify/<timestamp>/
```

The script re-enters `nix develop` by itself and diffs against `origin/master` plus uncommitted and untracked files (`--base REF` to change that). `--all` runs every gate. Naming gates (`scripts/verify.sh room browser`) runs exactly those. `scripts/verify.sh --help` is the full interface. The gate table, its triggers and commands live in the script. Read it there rather than trusting a summary.

A gate passes only on exit status 0. On FAIL the script prints the log's tail. Open the full log before you guess at a cause. A browser gate failure that names a timeout is still a failure. Rerun it once to check for a flake, and report both runs.

## Rules the script enforces, so don't work around them

- The browser gates rebuild `web/pkg` first. Never run `browser-smoke.cjs` by hand against an existing `web/pkg`. It may predate your Rust change, and then the run proves nothing.
- Each smoke run gets its own `p2skat-room` on a free port. Don't point it at a server someone else has open on 8080.
- Docs-only changes select no gate. That is the correct result, not a gap.

## What the script cannot cover

Do these by hand when the diff calls for them, and say in the report which you did and which you skipped.

- **Native play** (`src/net/`, `src/ui/`, `src/main.rs`). `cargo test` covers units, not the mesh setup or the TUI. Play a real round on localhost with one `cargo run -- host --port N` and two `cargo run -- join 127.0.0.1:N`, driven with the **control-cli** skill.
- **Tor** (`--tor`, `src/net/tor.rs`). It needs the live Tor network and takes minutes to bootstrap. Run host and join with `--tor` when Tor code changed.
- **Look and layout** (`web/style.css`, `web/index.html`). The smoke test checks desktop and mobile layouts structurally. For visual changes, take before and after screenshots with the **control-ui** skill.
- **The router.** `scripts/deploy-router.sh --check root@router` runs the conformance suite on the live onion host. It touches the user's network, so ask first. Deploying is never part of verification.
- **The real Tor Browser.** Acceptance 1 in `docs/plans/tor-onion-relay.md` needs three Tor Browsers on separate networks. Only the user can run it.

## Reporting

Quote the `summary.txt` lines (status, gate, seconds, log path) and the commit they ran on. List each manual check from the section above as done or skipped, with the reason. For a before/after claim ("this fixes X", "this is faster"), use the **verify-this** skill for the baseline and treatment. Run this script on both sides when it covers the claim.

## Adding a check

A new test surface becomes a gate in `scripts/verify.sh`. Add a `TRIGGER` regex, an `ABOUT` line and a `gate_<name>` function. Don't describe it here instead. A check written only in prose gets skipped. A check in the gate table runs whenever its files change.
