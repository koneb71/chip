<!-- Thanks for contributing to chip! Please fill out the sections below. -->

### What & why

What does this change do, and why? Link any related issue (e.g. `Closes #123`).

### How it was tested

How you verified the change — commands run, scenarios covered, new tests added.

### Checklist

- [ ] `cargo fmt --all` is clean
- [ ] `cargo clippy --all-targets --workspace -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] Docs updated if behavior, CLI, config, or the protocol changed
- [ ] No secrets, keys, or `.env` files committed
- [ ] Security-sensitive changes (auth, crypto, validation, sync) call out the
      threat addressed — see [SECURITY.md](../SECURITY.md)

By submitting this PR I agree my contribution is dual-licensed under
[MIT](../LICENSE-MIT) and [Apache-2.0](../LICENSE-APACHE).
