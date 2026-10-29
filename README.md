# Aster

Your terminal workspace for coding agents.

Aster is Sai Navaneet’s fork of [Herdr](https://github.com/herdrdev/herdr).
We are building it one step at a time, starting with the upstream terminal runtime.

## Run locally

```sh
aster --session sainavaneet
```

The `aster` launcher runs this checkout’s debug build. Commands, configuration,
environment variables, plugin manifests and agent integrations use Aster names.
Development state lives in `~/.config/aster-dev`; existing Herdr data is preserved.
Background release and detection updates are disabled by default while the fork
establishes its own releases.

## Build and test

Requires the Rust toolchain pinned in `rust-toolchain.toml`, Zig 0.16.0,
`just`, `cargo-nextest`, and Bun.

```sh
ZIG=/path/to/zig-0.16.0/zig cargo build --locked
ZIG=/path/to/zig-0.16.0/zig just test
```

See [upstream attribution](README.upstream.md) for origin and licensing details.

## License and attribution

Based on Herdr, licensed under Apache-2.0. Existing copyright notices,
licenses, and upstream attribution are preserved. See [LICENSE](LICENSE).
