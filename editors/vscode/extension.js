// VS Code client for `sijoc lsp` (IntelliSense) plus live-coding / explain commands.
const vscode = require("vscode");
const { LanguageClient, TransportKind } = require("vscode-languageclient/node");

let client;

function compilerPath() {
  return vscode.workspace.getConfiguration("sijo").get("compilerPath") || "sijoc";
}

function activate(context) {
  const serverOptions = {
    run: { command: compilerPath(), args: ["lsp"], transport: TransportKind.stdio },
    debug: { command: compilerPath(), args: ["lsp"], transport: TransportKind.stdio },
  };
  const clientOptions = { documentSelector: [{ scheme: "file", language: "sijo" }] };
  client = new LanguageClient("sijo", "Sijo Language Server", serverOptions, clientOptions);
  client.start();

  const runInTerminal = (name, args) => {
    const editor = vscode.window.activeTextEditor;
    if (!editor || editor.document.languageId !== "sijo") {
      vscode.window.showWarningMessage("Open a .sijo file first.");
      return;
    }
    editor.document.save();
    const term = vscode.window.createTerminal(name);
    term.show();
    term.sendText(`${compilerPath()} ${args} "${editor.document.fileName}"`);
  };
  context.subscriptions.push(
    vscode.commands.registerCommand("sijo.live", () => runInTerminal("Sijo Live", "live")),
    vscode.commands.registerCommand("sijo.explain", () => runInTerminal("Sijo Explain", "explain")),
    vscode.commands.registerCommand("sijo.fixAll", async () => {
      await vscode.commands.executeCommand("editor.action.sourceAction", { kind: "source.fixAll", apply: "first" });
    })
  );
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
