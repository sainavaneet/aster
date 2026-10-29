# aster


<p align="center">
  <img src="assets/logo.png" alt="aster" width="100" />
</p>

<p align="center">
  <a href="https://github.com/sainavaneet/aster">github.com/sainavaneet/aster</a> · <a href="#install">install</a> · <a href="https://github.com/sainavaneet/aster/docs/quick-start/">quick start</a> · <a href="https://github.com/sainavaneet/aster/docs/">docs</a>
</p>

<p align="center">
  English · <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-666666?labelColor=333333" alt="Apache 2.0 license" /></a>
  <a href="https://github.com/sainavaneet/aster/releases"><img src="https://img.shields.io/github/downloads/sainavaneet/aster/total?labelColor=333333&color=666666" alt="total GitHub release downloads" /></a>
  <a href="https://github.com/sainavaneet/aster/stargazers"><img src="https://img.shields.io/github/stars/sainavaneet/aster?labelColor=333333&color=666666&logo=github" alt="GitHub stars" /></a>
  <a href="https://github.com/sainavaneet/aster/releases/latest"><img src="https://img.shields.io/github/v/release/sainavaneet/aster?label=release&labelColor=333333&color=666666" alt="latest stable release" /></a>
  <a href="https://formulae.brew.sh/formula/aster"><img src="https://img.shields.io/homebrew/v/aster?label=homebrew&labelColor=333333&color=666666" alt="Homebrew version" /></a>
  <a href="https://x.com/sainavaneet"><img src="https://img.shields.io/badge/follow-%40sainavaneet-000000?logo=x&logoColor=white" alt="follow @sainavaneet on X" /></a>
</p>

---

https://github.com/user-attachments/assets/043ec09f-4bdd-41d5-aee0-8fda6b83e267

**the runtime your coding agents live on.**

- **detach without stopping work** — aster keeps terminals running in a background server when you close the client or lose your SSH connection. after a server or machine restart, aster restores the saved layout and can resume supported agent sessions; the original processes do not survive. [session state →](https://github.com/sainavaneet/aster/docs/session-state/)
- **several machines, one window** — keep local work and saved ssh machines together, with a combined agent list and independent reconnects. [remote machines →](https://github.com/sainavaneet/aster/docs/connecting-machines/)
- **never hunt for the stuck one** — every pane is marked working, blocked, or idle. when an agent stops and needs an answer, aster says so.
- **agent-native** — agents drive aster through the cli and socket api: they can spawn panes, prompt each other, and wait until another agent is genuinely blocked. [agent skill →](https://github.com/sainavaneet/aster/docs/agent-skill/)
- **runs what you already run** — claude code, codex, cursor, opencode, grok and the rest. aster doesn't wrap or replace them; it owns their terminals. building an agent? [add aster support →](https://github.com/sainavaneet/aster/docs/add-aster-support/)
- **keyboard and mouse, both first-class** — tmux-style prefix keys *and* click, drag, split. pick per moment, not per tool.
- **plugins** — extend panes and workflows. [browse the marketplace →](https://github.com/sainavaneet/aster/plugins/)
- **one rust binary, no electron** — runs in whatever terminal you already use.

---

## install

```bash
curl -fsSL https://github.com/sainavaneet/aster/install.sh | sh
```

or `brew install aster` · `mise use -g aster` · windows: `powershell -ExecutionPolicy Bypass -c "irm https://github.com/sainavaneet/aster/install.ps1 | iex"` · [endpoint-protected Windows](https://github.com/sainavaneet/aster/docs/windows-beta/) · [binaries](https://github.com/sainavaneet/aster/releases)

then start it where the work lives:

```bash
aster
```

run your agents, split panes, walk away. `ctrl+b q` detaches, `aster` reattaches. [quick start →](https://github.com/sainavaneet/aster/docs/quick-start/)

## docs

everything lives at [github.com/sainavaneet/aster/docs](https://github.com/sainavaneet/aster/docs/): [quick start](https://github.com/sainavaneet/aster/docs/quick-start/) · [concepts](https://github.com/sainavaneet/aster/docs/concepts/) · [supported agents](https://github.com/sainavaneet/aster/docs/agents/) · [keyboard](https://github.com/sainavaneet/aster/docs/keyboard/) · [configuration](https://github.com/sainavaneet/aster/docs/configuration/) · [session state](https://github.com/sainavaneet/aster/docs/session-state/) · [connecting machines](https://github.com/sainavaneet/aster/docs/connecting-machines/) · [remote](https://github.com/sainavaneet/aster/docs/persistence-remote/) · [integrations](https://github.com/sainavaneet/aster/docs/integrations/) · [add aster support to your agent](https://github.com/sainavaneet/aster/docs/add-aster-support/) · [plugins](https://github.com/sainavaneet/aster/docs/plugins/) · [socket api](https://github.com/sainavaneet/aster/docs/socket-api/)

## thanks

every past sponsor and backer is listed in [SPONSORS.md](./SPONSORS.md) — thank you 🐑

enterprise / partnership: the repository Discussions page

## agent instructions

if you are an ai agent helping with this repository, read [`AGENTS.md`](./AGENTS.md) before making changes and read [`CONTRIBUTING.md`](./CONTRIBUTING.md) before opening issues or PRs.

## development

```bash
git clone https://github.com/sainavaneet/aster
cd aster
cargo build --release

just test        # unit tests
just check       # formatting, tests, and maintenance checks
```

## license

Aster is licensed under the [Apache License 2.0](LICENSE).
