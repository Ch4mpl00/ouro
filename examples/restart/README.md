# Restart example

```sh
cargo run -p ouro-restart-example
```

Registers a read and a write, saves and activates YAML, admits a run, kills a
worker after the write result commits, then recovers through a fresh runtime.
The local ledger contains one write. The example advances an injected recovery
clock past lease expiry so it completes without a 30-second pause.
