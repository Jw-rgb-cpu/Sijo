# Sijo for Visual Studio Code

- **IntelliSense** via `sijoc lsp`: completion, hover (with the compiler's decisions: ECS component,
  replication bits, range contracts, memory region, system stage), go to definition, find references,
  rename, document outline, inlay hints.
- **Automatic lint** (Sijo coding standard / .NET / clang-tidy rules + Sijo game rules) on every keystroke,
  quick fixes and *Fix All* (`Sijo: Fix All Lint Issues`).
- **Live coding**: `Sijo: Start Live Coding` runs `sijoc live` — save the file and the running game
  continues with the new code and its state preserved.

## Setup

```bash
cd compiler && cargo build --release      # builds sijoc
cd ../editors/vscode && npm install       # fetches vscode-languageclient
code --extensionDevelopmentPath=$PWD      # or package with `npx @vscode/vsce package`
```

Set `sijo.compilerPath` if `sijoc` is not on your PATH. Any other LSP editor (Neovim, Helix, Emacs,
Sublime) can use `sijoc lsp` directly.
